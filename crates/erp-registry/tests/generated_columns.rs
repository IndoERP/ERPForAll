//! Acceptance tests for database-generated scalar fields.
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use erp_fields::{ErrorKind, FieldType, IndexKind, RangeBounds};
use erp_registry::{
    AddonDescriptor, ExclusionDecl, FieldDecl, FieldDef, GeneratedStorage, IndexDecl, IndexMethod,
    KeyDecl, KindDef, ModelDecl, ModelDef, Overlay, PartitionDecl, PartitionInterval,
    PartitionRetain, Registry, RegistryBuilder, StaticValue, TableObjectDecl, TableObjectKind,
};

static BASE_ADDON: AddonDescriptor = AddonDescriptor {
    name: "base",
    depends: &[],
    exact_only: false,
};

static TEST_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_sqlfields",
    depends: &["base"],
    exact_only: false,
};

static EXT_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_sqlfields_ext",
    depends: &["test_sqlfields"],
    exact_only: false,
};

fn leak_decl(d: ModelDecl) -> &'static ModelDecl {
    Box::leak(Box::new(d))
}

fn key(name: &'static str, decl: KeyDecl) -> TableObjectDecl {
    TableObjectDecl {
        name,
        kind: TableObjectKind::Key(decl),
    }
}

fn exclusion(name: &'static str, decl: ExclusionDecl) -> TableObjectDecl {
    TableObjectDecl {
        name,
        kind: TableObjectKind::Exclusion(decl),
    }
}

fn build(
    addons: &[&'static AddonDescriptor],
    decls: impl IntoIterator<Item = ModelDecl>,
) -> erp_fields::Result<Registry> {
    let leaked: Vec<&'static ModelDecl> = decls.into_iter().map(leak_decl).collect();
    RegistryBuilder::new(addons.iter().copied())
        .with_decls(leaked)
        .build()
}

#[test]
fn spec_sqlfield_001_declaration_surfaces_agree() {
    // 1. Native declarations
    static NATIVE_MODEL: [ModelDecl; 1] = [ModelDecl::new("test_sqlfields", "sample.native")
        .fields(
            const {
                &[
                    FieldDecl::integer("a"),
                    FieldDecl::integer("b"),
                    // Omitted storage -> VIRTUAL
                    FieldDecl::integer("total").generated("a + b"),
                    // Explicit virtual -> VIRTUAL
                    FieldDecl::integer("v_total")
                        .generated("a + b")
                        .generated_storage(GeneratedStorage::Virtual),
                    // Explicit stored -> STORED
                    FieldDecl::integer("s_total")
                        .generated("a + b")
                        .generated_storage(GeneratedStorage::Stored),
                ]
            },
        )];

    let reg = RegistryBuilder::new([&BASE_ADDON, &TEST_ADDON])
        .with_decls(NATIVE_MODEL.iter())
        .build()
        .unwrap();

    let model = reg.model("sample.native").unwrap();
    let fa = model.field("a").unwrap();
    let fb = model.field("b").unwrap();
    let total = model.field("total").unwrap();
    let v_total = model.field("v_total").unwrap();
    let s_total = model.field("s_total").unwrap();

    assert_eq!(total.ty, FieldType::Integer);
    assert!(total.is_generated());
    assert!(total.is_virtual());
    assert!(!total.is_stored_generated());
    assert!(total.store);
    assert!(total.readonly);
    assert!(!total.copy);
    assert!(total.has_column());
    assert!(!total.is_assignment_eligible());
    assert!(!total.is_computed());
    assert_eq!(total.generated().unwrap().dependencies(), &[fa.id, fb.id]);

    assert!(v_total.is_generated());
    assert!(v_total.is_virtual());
    assert_eq!(
        v_total.generated().unwrap().storage(),
        GeneratedStorage::Virtual
    );
    assert_eq!(v_total.generated().unwrap().dependencies(), &[fa.id, fb.id]);

    assert!(s_total.is_generated());
    assert!(!s_total.is_virtual());
    assert!(s_total.is_stored_generated());
    assert_eq!(
        s_total.generated().unwrap().storage(),
        GeneratedStorage::Stored
    );
    assert_eq!(s_total.generated().unwrap().dependencies(), &[fa.id, fb.id]);

    // 2. Owned / runtime definitions (FieldDef) with to_decl()
    let mut owned_model = ModelDef::new("test_sqlfields", "sample.owned");
    owned_model
        .fields
        .push(FieldDef::new("a", KindDef::Integer));
    owned_model
        .fields
        .push(FieldDef::new("b", KindDef::Integer));
    owned_model
        .fields
        .push(FieldDef::new("total", KindDef::Integer).generated("a + b"));
    owned_model.fields.push(
        FieldDef::new("s_total", KindDef::Integer)
            .generated("a + b")
            .generated_storage(GeneratedStorage::Stored),
    );

    let owned_decl = owned_model.to_decl().unwrap();
    let reg_owned = RegistryBuilder::new([&BASE_ADDON, &TEST_ADDON])
        .with_decls([owned_decl])
        .build()
        .unwrap();

    let m_owned = reg_owned.model("sample.owned").unwrap();
    let ototal = m_owned.field("total").unwrap();
    let os_total = m_owned.field("s_total").unwrap();
    assert!(ototal.is_virtual());
    assert!(os_total.is_stored_generated());
    assert_eq!(ototal.generated().unwrap().source(), "a + b");
    assert_eq!(os_total.generated().unwrap().source(), "a + b");

    // 3. Owned definitions through per-database Overlay
    let mut overlay = Overlay::new();
    let mut ov_model = ModelDef::new("test_sqlfields", "sample.overlay");
    ov_model.fields.push(FieldDef::new("x", KindDef::Integer));
    ov_model.fields.push(
        FieldDef::new("x_gen", KindDef::Integer)
            .generated("x + 42")
            .generated_storage(GeneratedStorage::Virtual),
    );
    overlay.add_addon_defs([ov_model]).unwrap();

    let reg_ov = RegistryBuilder::new([&BASE_ADDON, &TEST_ADDON])
        .overlay(&overlay)
        .build()
        .unwrap();
    let m_ov = reg_ov.model("sample.overlay").unwrap();
    let x_gen = m_ov.field("x_gen").unwrap();
    assert!(x_gen.is_virtual());
    assert_eq!(x_gen.generated().unwrap().source(), "x + 42");
}

/// Prohibited attributes on a generated output fail validation with Programming and
/// stable error messages.
#[test]
fn spec_sqlfield_002_conflicting_attributes_fail() {
    let check_fails = |setup_field: fn(FieldDecl) -> FieldDecl, expected_reason: &str| {
        let field = setup_field(FieldDecl::integer("f").generated("a + 1"));
        let decl = ModelDecl::new("test_sqlfields", "sample.conflict").fields(Box::leak(
            vec![FieldDecl::integer("a"), field].into_boxed_slice(),
        ));
        let err = build(&[&BASE_ADDON, &TEST_ADDON], [decl]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        let msg = err.message();
        let expected = format!("Invalid generated field 'sample.conflict.f': {expected_reason}");
        assert!(
            msg.contains(&expected),
            "Expected error containing {expected:?}, got {msg:?}"
        );
    };

    check_fails(
        |f| f.readonly(false),
        "attribute 'readonly' conflicts with generation",
    );
    check_fails(
        |f| f.copy(true),
        "attribute 'copy' conflicts with generation",
    );
    check_fails(
        |f| f.store(false),
        "attribute 'store' conflicts with generation",
    );
    check_fails(|f| f.required(true), "required is not supported");
    check_fails(
        |f| f.default(StaticValue::Int(42)),
        "attribute 'default' conflicts with generation",
    );
    check_fails(
        |f| f.compute("_compute_f"),
        "attribute 'compute' conflicts with generation",
    );
    check_fails(
        |f| f.related("a"),
        "attribute 'related' conflicts with generation",
    );
    check_fails(
        |f| f.inverse("_inverse_f"),
        "attribute 'inverse' conflicts with generation",
    );
    check_fails(
        |f| f.precompute(true),
        "attribute 'precompute' conflicts with generation",
    );
    check_fails(
        |f| f.compute_sudo(true),
        "attribute 'compute_sudo' conflicts with generation",
    );
    check_fails(
        |f| f.depends(&["a"]),
        "attribute 'depends' conflicts with generation",
    );
    check_fails(
        |f| f.depends_context(&["lang"]),
        "attribute 'depends_context' conflicts with generation",
    );

    // Wrong and empty storage values: the typed surface holds an enum, so the only place a text
    // value exists is `GeneratedStorage::parse`, which the model-file surface calls.
    for bad in [
        "",
        " ",
        "bogus",
        "stored_",
        "virtual stored",
        "vir tual",
        "\0",
    ] {
        assert_eq!(GeneratedStorage::parse(bad), None, "{bad:?} must not parse");
    }
    assert_eq!(
        GeneratedStorage::parse("virtual"),
        Some(GeneratedStorage::Virtual)
    );
    assert_eq!(
        GeneratedStorage::parse("STORED"),
        Some(GeneratedStorage::Stored)
    );

    // Storage attribute without expression
    let field_no_expr = FieldDecl::integer("f").generated_storage(GeneratedStorage::Virtual);
    let decl_no_expr = ModelDecl::new("test_sqlfields", "sample.no_expr")
        .fields(Box::leak(vec![field_no_expr].into_boxed_slice()));
    let err = build(&[&BASE_ADDON, &TEST_ADDON], [decl_no_expr]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("Invalid generated field 'sample.no_expr.f': attribute 'generated_storage' conflicts with generation"));
}

/// Expression references and cycles fail before SQL.
#[test]
fn spec_sqlfield_003_expression_references_and_cycles_fail() {
    let check_refusal = |fields: Vec<FieldDecl>, expected_reason: &str| {
        let decl = ModelDecl::new("test_sqlfields", "sample.refs")
            .fields(Box::leak(fields.into_boxed_slice()));
        let err = build(&[&BASE_ADDON, &TEST_ADDON], [decl]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        let msg = err.message();
        let expected = format!("Invalid generated field 'sample.refs.total': {expected_reason}");
        assert!(
            msg.contains(&expected),
            "Expected {expected:?}, got {msg:?}"
        );
    };

    // Unknown source
    check_refusal(
        vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("total").generated("a + nonexistent"),
        ],
        "unknown source 'nonexistent'",
    );

    // Self reference
    check_refusal(
        vec![FieldDecl::integer("total").generated("total + 1")],
        "source 'total' is not an ordinary local column",
    );

    // Generated source dependency
    check_refusal(
        vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("sub").generated("a + 1"),
            FieldDecl::integer("total").generated("sub + 1"),
        ],
        "source 'sub' is not an ordinary local column",
    );

    // Callback computed source dependency
    check_refusal(
        vec![
            FieldDecl::integer("comp").compute("_compute_val"),
            FieldDecl::integer("total").generated("comp + 1"),
        ],
        "source 'comp' is not an ordinary local column",
    );

    // Related source dependency
    check_refusal(
        vec![
            FieldDecl::many2one("partner_id", "sample.refs"),
            FieldDecl::integer("rel").related("partner_id.id"),
            FieldDecl::integer("total").generated("rel + 1"),
        ],
        "source 'rel' is not an ordinary local column",
    );

    // Indirect dependency cycle: generated sources are refused, so a cycle can never reach SQL
    check_refusal(
        vec![
            FieldDecl::integer("total").generated("other + 1"),
            FieldDecl::integer("other").generated("total + 1"),
        ],
        "source 'other' is not an ordinary local column",
    );
    check_refusal(
        vec![
            FieldDecl::integer("total").generated("mid + 1"),
            FieldDecl::integer("mid").generated("last + 1"),
            FieldDecl::integer("last").generated("total + 1"),
        ],
        "source 'mid' is not an ordinary local column",
    );

    // Cross-row access (path through a relation, subscript) is outside the grammar
    check_refusal(
        vec![
            FieldDecl::many2one("partner_id", "sample.refs"),
            FieldDecl::integer("total").generated("partner_id.id + 1"),
        ],
        "unsupported expression",
    );
    check_refusal(
        vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("total").generated("a[0] + 1"),
        ],
        "unsupported expression",
    );

    // Delegated field from _inherits parent
    let parent_decl = ModelDecl::new("test_sqlfields", "sample.parent")
        .fields(const { &[FieldDecl::integer("parent_col")] });
    let child_decl = ModelDecl::new("test_sqlfields", "sample.refs")
        .inherits(&[("sample.parent", "parent_id")])
        .fields(Box::leak(
            vec![
                FieldDecl::many2one("parent_id", "sample.parent")
                    .required(true)
                    .ondelete(erp_fields::OnDelete::Cascade)
                    .delegate(true),
                FieldDecl::integer("total").generated("parent_col + 1"),
            ]
            .into_boxed_slice(),
        ));

    let err = build(&[&BASE_ADDON, &TEST_ADDON], [parent_decl, child_decl]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains("Invalid generated field 'sample.refs.total': source 'parent_col' is not an ordinary local column"),
        "GOT ERROR: {:?}", err.message()
    );
}

/// Bounded scalar expression language supports arithmetic, boolean logic, comparisons,
/// conditional expressions and closed safe functions; rejects arbitrary functions,
/// attributes, containers and syntax injection. Emitted SQL quotes identifiers and qualifies
/// functions with pg_catalog.
#[test]
fn spec_sqlfield_004_language_and_injection_boundary() {
    let check_valid_sql = |expr_str: &'static str, expected_snippet: &str| {
        let decl = ModelDecl::new("test_sqlfields", "sample.lang").fields(Box::leak(
            vec![
                FieldDecl::integer("a"),
                FieldDecl::integer("b"),
                FieldDecl::boolean("flag"),
                FieldDecl::text("label"),
                FieldDecl::integer("res").generated(expr_str),
            ]
            .into_boxed_slice(),
        ));
        let reg = build(&[&BASE_ADDON, &TEST_ADDON], [decl]).unwrap();
        let model = reg.model("sample.lang").unwrap();
        let res = model.field("res").unwrap();
        let sql = res.generated().unwrap().sql();
        assert!(
            sql.contains(expected_snippet),
            "Expected SQL snippet {expected_snippet:?} in {sql:?}"
        );
    };

    // Arithmetic
    // Operators are schema-qualified so a public-schema overload cannot be selected.
    check_valid_sql("a + b", "(\"a\" OPERATOR(pg_catalog.+) \"b\")");
    check_valid_sql("a - b", "(\"a\" OPERATOR(pg_catalog.-) \"b\")");
    check_valid_sql("a * b", "(\"a\" OPERATOR(pg_catalog.*) \"b\")");
    check_valid_sql("+a", "(OPERATOR(pg_catalog.+) \"a\")");
    check_valid_sql("-a", "(OPERATOR(pg_catalog.-) \"a\")");

    // Comparisons yield a boolean, so they need a boolean target.
    for (expr_str, snippet) in [
        ("a == b", "(\"a\" OPERATOR(pg_catalog.=) \"b\")"),
        ("a != b", "(\"a\" OPERATOR(pg_catalog.<>) \"b\")"),
        ("a >= b", "(\"a\" OPERATOR(pg_catalog.>=) \"b\")"),
    ] {
        let decl = ModelDecl::new("test_sqlfields", "sample.cmp").fields(Box::leak(
            vec![
                FieldDecl::integer("a"),
                FieldDecl::integer("b"),
                FieldDecl::boolean("res").generated(expr_str),
            ]
            .into_boxed_slice(),
        ));
        let reg = build(&[&BASE_ADDON, &TEST_ADDON], [decl]).unwrap();
        let sql = reg
            .model("sample.cmp")
            .unwrap()
            .field("res")
            .unwrap()
            .generated()
            .unwrap()
            .sql();
        assert!(sql.contains(snippet), "expected {snippet:?} in {sql:?}");
    }

    // Conditional
    check_valid_sql(
        "a if flag else b",
        "(CASE WHEN \"flag\" THEN \"a\" ELSE \"b\" END)",
    );

    // pg_proc functions are pg_catalog-qualified; LEAST/GREATEST/COALESCE are SQL grammar forms
    // that PostgreSQL rejects when qualified, so they render bare (review D1).
    check_valid_sql("least(a, b)", "LEAST(\"a\", \"b\")");
    check_valid_sql("greatest(a, b)", "GREATEST(\"a\", \"b\")");
    check_valid_sql("coalesce(a, b, 0)", "COALESCE(\"a\", \"b\", 0)");
    check_valid_sql("abs(a)", "pg_catalog.abs(\"a\")");

    // Text expressions
    let decl_text = ModelDecl::new("test_sqlfields", "sample.lang_text").fields(Box::leak(
        vec![
            FieldDecl::text("label"),
            FieldDecl::text("res1").generated("lower(label)"),
            FieldDecl::text("res2").generated("upper(label)"),
            FieldDecl::text("res3").generated("'O\\'Reilly'"),
        ]
        .into_boxed_slice(),
    ));
    let reg_text = build(&[&BASE_ADDON, &TEST_ADDON], [decl_text]).unwrap();
    let m = reg_text.model("sample.lang_text").unwrap();
    assert_eq!(
        m.field("res1").unwrap().generated().unwrap().sql(),
        "pg_catalog.lower(\"label\")"
    );
    assert_eq!(
        m.field("res2").unwrap().generated().unwrap().sql(),
        "pg_catalog.upper(\"label\")"
    );
    assert_eq!(
        m.field("res3").unwrap().generated().unwrap().sql(),
        "'O''Reilly'"
    );

    // Disallowed language constructs: division, arbitrary calls, floats, attributes, containers
    let check_rejected = |expr_str: &'static str| {
        let decl = ModelDecl::new("test_sqlfields", "sample.bad").fields(Box::leak(
            vec![
                FieldDecl::integer("a"),
                FieldDecl::integer("b"),
                FieldDecl::integer("res").generated(expr_str),
            ]
            .into_boxed_slice(),
        ));
        let err = build(&[&BASE_ADDON, &TEST_ADDON], [decl]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert!(
            err.message()
                .contains("Invalid generated field 'sample.bad.res': unsupported expression")
        );
    };

    check_rejected("a / b");
    check_rejected("a // b");
    check_rejected("a % b");
    check_rejected("a & b");
    check_rejected("a | b");
    check_rejected("2.5");
    check_rejected("sin(a)");
    check_rejected("a.attribute");
    check_rejected("[a, b]");
    check_rejected("(a, b)");
    check_rejected("least(a, b, kw=1)");
    check_rejected("abs(a, b)"); // wrong number of arguments
}

/// Enforces source size (16 KiB), depth (32), node count (1024), SQL output (64 KiB),
/// and strict type compatibility between operands and declared output kind.
#[test]
fn spec_sqlfield_005_bounds_and_types() {
    let check_err = |expr_str: &'static str, expected_fragment: &str| {
        let decl = ModelDecl::new("test_sqlfields", "sample.bounds").fields(Box::leak(
            vec![
                FieldDecl::integer("a"),
                FieldDecl::integer("b"),
                FieldDecl::boolean("flag"),
                FieldDecl::text("label"),
                FieldDecl::integer("res").generated(expr_str),
            ]
            .into_boxed_slice(),
        ));
        let err = build(&[&BASE_ADDON, &TEST_ADDON], [decl]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert!(
            err.message().contains(expected_fragment),
            "Expected fragment {expected_fragment:?}, got {:?}",
            err.message()
        );
    };

    // 1. Source size limit > 16 KiB
    let huge_expr = format!("a + {}", "1 + ".repeat(6000));
    let huge_static: &'static str = Box::leak(huge_expr.into_boxed_str());
    check_err(huge_static, "expression exceeds 16384 bytes source limit");

    // 2. Depth limit > 32
    let mut deep_expr = "a".to_string();
    for _ in 0..40 {
        deep_expr = format!("(-{deep_expr})");
    }
    let deep_static: &'static str = Box::leak(deep_expr.into_boxed_str());
    check_err(deep_static, "depth limit");

    // 3. Node limit > 1024
    fn balanced_expr(depth: usize) -> String {
        if depth == 1 {
            "a".to_string()
        } else {
            format!(
                "({} + {})",
                balanced_expr(depth - 1),
                balanced_expr(depth - 1)
            )
        }
    }
    let wide_expr = balanced_expr(11);
    let wide_static: &'static str = Box::leak(wide_expr.into_boxed_str());
    check_err(wide_static, "nodes limit");

    // 4. Type mismatch: boolean operator on integers
    check_err("a and b", "unsupported expression");

    // 5. Type mismatch: arithmetic operator on text
    check_err("label + 1", "unsupported expression");

    // 6. Type mismatch: integer result assigned to boolean field or vice versa
    let decl_type_mismatch = ModelDecl::new("test_sqlfields", "sample.type_mismatch").fields(
        const {
            &[
                FieldDecl::integer("a"),
                FieldDecl::boolean("res").generated("a + 1"),
            ]
        },
    );
    let err = build(&[&BASE_ADDON, &TEST_ADDON], [decl_type_mismatch]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message()
            .contains("Invalid generated field 'sample.type_mismatch.res': unsupported expression")
    );
}

/// Inherited declarations preserve expression and storage when absent; replacement
/// replaces expression and updates storage; removal is refused with stable error.
#[test]
fn spec_sqlfield_006_inheritance_and_declaration_replacement() {
    // 1. Base model with generated field
    static BASE_MODEL: [ModelDecl; 1] = [ModelDecl::new("test_sqlfields", "sample.inherit")
        .fields(
            const {
                &[
                    FieldDecl::integer("a"),
                    FieldDecl::integer("b"),
                    FieldDecl::integer("total")
                        .generated("a + b")
                        .generated_storage(GeneratedStorage::Virtual),
                ]
            },
        )];

    // 2. Extension that does not mention total inherits expression and storage
    static EXT1: [ModelDecl; 1] =
        [
            ModelDecl::extension("test_sqlfields_ext", "sample.inherit", &["sample.inherit"])
                .fields(const { &[FieldDecl::char("c")] }),
        ];

    let reg = RegistryBuilder::new([&BASE_ADDON, &TEST_ADDON, &EXT_ADDON])
        .with_decls(BASE_MODEL.iter().chain(EXT1.iter()))
        .build()
        .unwrap();

    let m1 = reg.model("sample.inherit").unwrap();
    let total1 = m1.field("total").unwrap();
    assert!(total1.is_virtual());
    assert_eq!(total1.generated().unwrap().source(), "a + b");

    // 3. Extension that replaces expression and keeps inherited storage
    static EXT2: [ModelDecl; 1] =
        [
            ModelDecl::extension("test_sqlfields_ext", "sample.inherit", &["sample.inherit"])
                .fields(const { &[FieldDecl::integer("total").generated("a * 2")] }),
        ];

    let reg2 = RegistryBuilder::new([&BASE_ADDON, &TEST_ADDON, &EXT_ADDON])
        .with_decls(BASE_MODEL.iter().chain(EXT2.iter()))
        .build()
        .unwrap();
    let m2 = reg2.model("sample.inherit").unwrap();
    let total2 = m2.field("total").unwrap();
    assert!(total2.is_virtual());
    assert_eq!(total2.generated().unwrap().source(), "a * 2");

    // 4. Extension that replaces expression AND updates storage to STORED
    static EXT3: [ModelDecl; 1] =
        [
            ModelDecl::extension("test_sqlfields_ext", "sample.inherit", &["sample.inherit"])
                .fields(
                    const {
                        &[FieldDecl::integer("total")
                            .generated("a * 3")
                            .generated_storage(GeneratedStorage::Stored)]
                    },
                ),
        ];

    let reg3 = RegistryBuilder::new([&BASE_ADDON, &TEST_ADDON, &EXT_ADDON])
        .with_decls(BASE_MODEL.iter().chain(EXT3.iter()))
        .build()
        .unwrap();
    let m3 = reg3.model("sample.inherit").unwrap();
    let total3 = m3.field("total").unwrap();
    assert!(total3.is_stored_generated());
    assert_eq!(total3.generated().unwrap().source(), "a * 3");

    // 5. Attempted removal is refused
    static EXT4: [ModelDecl; 1] =
        [
            ModelDecl::extension("test_sqlfields_ext", "sample.inherit", &["sample.inherit"])
                .fields(const { &[FieldDecl::integer("total").remove_generated()] }),
        ];

    let err = RegistryBuilder::new([&BASE_ADDON, &TEST_ADDON, &EXT_ADDON])
        .with_decls(BASE_MODEL.iter().chain(EXT4.iter()))
        .build()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains(
            "Invalid generated field 'sample.inherit.total': generation cannot be removed"
        )
    );
}

/// Refuses VIRTUAL fields used in field indexes, typed indexes, keys, exclusions,
/// temporal keys, and partition keys. STORED fields follow existing index rules.
#[test]
fn spec_sqlfield_007_virtual_index_and_constraint_refusals() {
    // 1. Field index on VIRTUAL field
    let decl_idx = ModelDecl::new("test_sqlfields", "sample.v_idx").fields(
        const {
            &[
                FieldDecl::integer("a"),
                FieldDecl::integer("v_col")
                    .generated("a + 1")
                    .index(Some(IndexKind::Btree)),
            ]
        },
    );
    let err = build(&[&BASE_ADDON, &TEST_ADDON], [decl_idx]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains(
        "Invalid generated field 'sample.v_idx.v_col': virtual field is referenced by 'index'"
    ));

    // 2. TypedIndex on VIRTUAL field
    let decl_typed_idx = ModelDecl::new("test_sqlfields", "sample.v_typed")
        .fields(
            const {
                &[
                    FieldDecl::integer("a"),
                    FieldDecl::integer("v_col").generated("a + 1"),
                ]
            },
        )
        .table_objects(
            const {
                &[TableObjectDecl::index(
                    "_v_btree",
                    IndexDecl::new(&["v_col"]).method(IndexMethod::Btree),
                )]
            },
        );
    let err = build(&[&BASE_ADDON, &TEST_ADDON], [decl_typed_idx]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("Invalid generated field 'sample.v_typed.v_col': virtual field is referenced by 'index '_v_btree''"));

    // 3. Business Key on VIRTUAL field
    let decl_key = ModelDecl::new("test_sqlfields", "sample.v_key")
        .fields(
            const {
                &[
                    FieldDecl::integer("a"),
                    FieldDecl::integer("v_col").generated("a + 1"),
                ]
            },
        )
        .table_objects(Box::leak(
            vec![key("_uniq_v", KeyDecl::new(&["v_col"], "Unique virtual"))].into_boxed_slice(),
        ));
    let err = build(&[&BASE_ADDON, &TEST_ADDON], [decl_key]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("Invalid generated field 'sample.v_key.v_col': virtual field is referenced by 'key '_uniq_v''"));

    // 4. Exclusion on VIRTUAL field
    let decl_excl = ModelDecl::new("test_sqlfields", "sample.v_excl")
        .fields(
            const {
                &[
                    FieldDecl::date("d_from"),
                    FieldDecl::date("d_to"),
                    FieldDecl::integer("v_col").generated("1"),
                ]
            },
        )
        .table_objects(Box::leak(
            vec![exclusion(
                "_excl_v",
                ExclusionDecl::new(("d_from", "d_to"), RangeBounds::Closed, "No overlap")
                    .equal(&["v_col"]),
            )]
            .into_boxed_slice(),
        ));
    let err = build(&[&BASE_ADDON, &TEST_ADDON], [decl_excl]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("Invalid generated field 'sample.v_excl.v_col': virtual field is referenced by 'exclusion '_excl_v''"));

    // 5. Partition key on generated field
    let decl_part = ModelDecl::new("test_sqlfields", "sample.part")
        .partition(
            PartitionDecl::new(PartitionInterval::Day, 7, PartitionRetain::All).by("gen_col"),
        )
        .fields(
            const {
                &[
                    FieldDecl::integer("a"),
                    FieldDecl::integer("gen_col").generated("a + 1"),
                ]
            },
        );
    let err = build(&[&BASE_ADDON, &TEST_ADDON], [decl_part]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains("Invalid generated field 'sample.part.gen_col': partition key cannot be a generated field"),
        "GOT ERROR: {:?}", err.message()
    );

    // 6. STORED field in TypedIndex and Key is ALLOWED
    let decl_stored = ModelDecl::new("test_sqlfields", "sample.stored_idx")
        .fields(
            const {
                &[
                    FieldDecl::integer("a"),
                    FieldDecl::integer("s_col")
                        .generated("a + 1")
                        .generated_storage(GeneratedStorage::Stored),
                ]
            },
        )
        .table_objects(Box::leak(
            vec![
                TableObjectDecl::index(
                    "_s_btree",
                    IndexDecl::new(&["s_col"]).method(IndexMethod::Btree),
                ),
                key("_s_key", KeyDecl::new(&["s_col"], "Unique stored")),
            ]
            .into_boxed_slice(),
        ));
    let reg = build(&[&BASE_ADDON, &TEST_ADDON], [decl_stored]).unwrap();
    let m = reg.model("sample.stored_idx").unwrap();
    let s_col = m.field("s_col").unwrap();
    assert!(s_col.is_stored_generated());
}

/// added by an extension is refused on the merged field, review D6)
#[test]
fn spec_sqlfield_002_base_attribute_plus_extension_generated_fails() {
    static BASE_COMPUTE: [ModelDecl; 1] = [ModelDecl::new("test_sqlfields", "sample.merge")
        .fields(
            const {
                &[
                    FieldDecl::integer("a"),
                    FieldDecl::integer("f").compute("_compute_f"),
                ]
            },
        )];
    static BASE_DEFAULT: [ModelDecl; 1] = [ModelDecl::new("test_sqlfields", "sample.merge")
        .fields(
            const {
                &[
                    FieldDecl::integer("a"),
                    FieldDecl::integer("f").default(StaticValue::Int(7)),
                ]
            },
        )];
    static BASE_REQUIRED: [ModelDecl; 1] = [ModelDecl::new("test_sqlfields", "sample.merge")
        .fields(
            const {
                &[
                    FieldDecl::integer("a"),
                    FieldDecl::integer("f").required(true),
                ]
            },
        )];
    static BASE_STORE: [ModelDecl; 1] = [ModelDecl::new("test_sqlfields", "sample.merge").fields(
        const {
            &[
                FieldDecl::integer("a"),
                FieldDecl::integer("f").store(false),
            ]
        },
    )];
    static EXT: [ModelDecl; 1] =
        [
            ModelDecl::extension("test_sqlfields_ext", "sample.merge", &["sample.merge"])
                .fields(const { &[FieldDecl::integer("f").generated("a + 1")] }),
        ];

    for (base, reason) in [
        (
            &BASE_COMPUTE,
            "attribute 'compute' conflicts with generation",
        ),
        (
            &BASE_DEFAULT,
            "attribute 'default' conflicts with generation",
        ),
        (&BASE_REQUIRED, "required is not supported"),
        (&BASE_STORE, "attribute 'store' conflicts with generation"),
    ] {
        let err = RegistryBuilder::new([&BASE_ADDON, &TEST_ADDON, &EXT_ADDON])
            .with_decls(base.iter().chain(EXT.iter()))
            .build()
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        let expected = format!("Invalid generated field 'sample.merge.f': {reason}");
        assert_eq!(err.message(), expected);
    }
}

#[test]
fn delegated_outputs_keep_owning_generation_metadata() {
    let parent = ModelDecl::new("test_sqlfields", "sample.generated_parent").fields(
        const {
            &[
                FieldDecl::integer("a"),
                FieldDecl::integer("total").generated("a + 1"),
            ]
        },
    );
    let child = ModelDecl::new("test_sqlfields", "sample.generated_child")
        .inherits(&[("sample.generated_parent", "parent_id")])
        .fields(
            const {
                &[FieldDecl::many2one("parent_id", "sample.generated_parent")
                    .required(true)
                    .delegate(true)
                    .ondelete(erp_fields::OnDelete::Cascade)]
            },
        );
    let grandchild = ModelDecl::new("test_sqlfields", "sample.generated_grandchild")
        .inherits(&[("sample.generated_child", "child_id")])
        .fields(
            const {
                &[FieldDecl::many2one("child_id", "sample.generated_child")
                    .required(true)
                    .delegate(true)
                    .ondelete(erp_fields::OnDelete::Cascade)]
            },
        );
    let registry = build(&[&BASE_ADDON, &TEST_ADDON], [grandchild, child, parent]).unwrap();
    let owner = registry
        .model("sample.generated_parent")
        .unwrap()
        .field("total")
        .unwrap()
        .generated()
        .unwrap();
    for model in ["sample.generated_child", "sample.generated_grandchild"] {
        let output = registry.model(model).unwrap().field("total").unwrap();
        assert!(output.inherited.is_some());
        assert!(output.column().is_none());
        assert!(!output.is_assignment_eligible());
        assert_eq!(output.generated().unwrap(), owner);
    }
}
