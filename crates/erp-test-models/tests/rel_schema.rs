//! Relational setup and schema (REL §4.3–4.7, MODEL-020…022): registry rules checked when the
//! registry is built, and the columns, relation tables and foreign keys schema sync creates.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, ModelKind};
use erp_sdk::{ErrorKind, FieldType, OnDelete};
use erp_testkit::Harness;
use support::rows;

static T: AddonDescriptor = AddonDescriptor {
    name: "rel_t",
    depends: &["base"],
    exact_only: false,
};

fn leak(fields: Vec<FieldDecl>) -> &'static [FieldDecl] {
    Box::leak(fields.into_boxed_slice())
}

fn model(name: &'static str, fields: Vec<FieldDecl>) -> ModelDecl {
    ModelDecl::new("rel_t", name).fields(leak(fields))
}

fn build(decls: Vec<ModelDecl>) -> Result<std::sync::Arc<erp_sdk::Registry>, erp_sdk::Error> {
    match Harness::builder([&T]).with_decls(decls).registry_only() {
        Ok(r) => Ok(r),
        Err(erp_testkit::HarnessError::Orm(e)) => Err(e),
        Err(other) => panic!("{other}"),
    }
}

fn ondelete(registry: &erp_sdk::Registry, model: &str, field: &str) -> OnDelete {
    match &registry.model(model).unwrap().field(field).unwrap().ty {
        FieldType::Many2one { ondelete, .. } => *ondelete,
        other => panic!("{other:?}"),
    }
}

#[test]
fn spec_rel_009_ondelete_defaults() {
    let regular = model(
        "x.regular",
        vec![
            FieldDecl::many2one("foo", "x.target")
                .required(true)
                .ondelete(OnDelete::Cascade),
            FieldDecl::many2one("bar", "x.target").required(true),
            FieldDecl::many2one("opt", "x.target"),
        ],
    );
    let transient = model(
        "x.wizard",
        vec![
            FieldDecl::many2one("foo", "x.target")
                .required(true)
                .ondelete(OnDelete::Restrict),
            FieldDecl::many2one("bar", "x.target").required(true),
            FieldDecl::many2one("opt", "x.target"),
        ],
    )
    .kind(ModelKind::Transient);
    let target = model("x.target", vec![]);
    let registry = build(vec![regular, transient, target]).unwrap();
    assert_eq!(ondelete(&registry, "x.regular", "foo"), OnDelete::Cascade);
    assert_eq!(ondelete(&registry, "x.regular", "bar"), OnDelete::Restrict);
    assert_eq!(ondelete(&registry, "x.regular", "opt"), OnDelete::SetNull);
    assert_eq!(ondelete(&registry, "x.wizard", "foo"), OnDelete::Restrict);
    assert_eq!(ondelete(&registry, "x.wizard", "bar"), OnDelete::Cascade);
    assert_eq!(ondelete(&registry, "x.wizard", "opt"), OnDelete::SetNull);
}

#[test]
fn spec_rel_010_invalid_ondelete_declarations() {
    let err = build(vec![model(
        "x.m",
        vec![
            FieldDecl::many2one("foo", "x.m")
                .required(true)
                .ondelete(OnDelete::SetNull),
        ],
    )])
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "The m2o field foo of model x.m is required but declares its ondelete policy as being \
         'set null'. Only 'restrict' and 'cascade' make sense."
    );
    let err = build(vec![
        model(
            "x.m",
            vec![FieldDecl::many2one("model_id", "ir.model").ondelete(OnDelete::Restrict)],
        ),
        model("ir.model", vec![]),
    ])
    .unwrap_err();
    assert_eq!(
        err.message(),
        "Field model_id of model x.m is defined as ondelete='restrict' while having ir.model as \
         comodel, the 'restrict' mode is not supported for this type of field as comodel."
    );
}

#[test]
fn spec_rel_011_many2one_to_transient_model() {
    let wizard = || model("some.transient", vec![]).kind(ModelKind::Transient);
    let err = build(vec![
        model(
            "x.m",
            vec![FieldDecl::many2one("wizard_id", "some.transient")],
        ),
        wizard(),
    ])
    .unwrap_err();
    assert_eq!(
        err.message(),
        "Many2one x.m.wizard_id from Model to TransientModel is forbidden"
    );
    // the reverse direction is accepted
    build(vec![
        model("x.m", vec![]),
        model("some.transient", vec![FieldDecl::many2one("m_id", "x.m")])
            .kind(ModelKind::Transient),
    ])
    .unwrap();
}

#[test]
fn spec_rel_019_delegate_without_inherits() {
    let err = build(vec![
        model(
            "x.m",
            vec![FieldDecl::many2one("partner_id", "x.partner").delegate(true)],
        ),
        model("x.partner", vec![]),
    ])
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "The delegate field x.m.partner_id must be declared in the model class e.g.\n_inherits = \
         {'x.partner': 'partner_id'}"
    );
}

#[test]
fn spec_rel_022_one2many_inverse_must_exist() {
    let err = build(vec![
        model(
            "x.base",
            vec![FieldDecl::one2many("line_ids", "x.line", "missing_id")],
        ),
        model("x.line", vec![FieldDecl::many2one("base_id", "x.base")]),
    ])
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "'missing_id' declared in x.base.line_ids does not exist on 'x.line'."
    );
}

#[test]
fn spec_rel_030_implicit_many2many_naming() {
    let registry = build(vec![
        model("b.x", vec![FieldDecl::many2many("ys", "a.y")]),
        model("a.y", vec![]),
        model("c.line", vec![FieldDecl::many2many("tags", "c.tag")]),
        model("c.line2", vec![FieldDecl::many2many("tags", "c.tag")]),
        model("c.tag", vec![]),
    ])
    .unwrap();
    let relation = |m: &str, f: &str| match &registry.model(m).unwrap().field(f).unwrap().ty {
        FieldType::Many2many {
            relation: Some(r), ..
        } => (r.table.clone(), r.column1.clone(), r.column2.clone()),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        relation("c.line", "tags"),
        (
            "c_line_c_tag_rel".to_owned(),
            "c_line_id".to_owned(),
            "c_tag_id".to_owned()
        )
    );
    assert_eq!(
        relation("c.line2", "tags"),
        (
            "c_line2_c_tag_rel".to_owned(),
            "c_line2_id".to_owned(),
            "c_tag_id".to_owned()
        )
    );
    // the table pair is sorted alphabetically
    assert_eq!(
        relation("b.x", "ys"),
        (
            "a_y_b_x_rel".to_owned(),
            "b_x_id".to_owned(),
            "a_y_id".to_owned()
        )
    );
}

#[test]
fn spec_rel_031_many2many_naming_errors() {
    let err = build(vec![model(
        "same.model",
        vec![FieldDecl::many2many("others", "same.model")],
    )])
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "same.model.others: Implicit/canonical naming of many2many relationship table is not \
         possible when source and destination models are the same"
    );
    let err = build(vec![
        model(
            "x.m",
            vec![FieldDecl::many2many("tags", "x.t").relation("Bad-Name", "a", "b")],
        ),
        model("x.t", vec![]),
    ])
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "Invalid characters in table name 'Bad-Name'");
    let long_a: &'static str = Box::leak("a".repeat(30).into_boxed_str());
    let long_b: &'static str = Box::leak("b".repeat(30).into_boxed_str());
    let err = build(vec![
        model(long_a, vec![FieldDecl::many2many("bs", long_b)]),
        model(long_b, vec![]),
    ])
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!("Table name '{long_a}_{long_b}_rel' is too long")
    );
}

#[test]
fn spec_rel_032_two_fields_on_one_relation() {
    let err = build(vec![
        model(
            "x.m",
            vec![
                FieldDecl::many2many("tags", "x.t"),
                FieldDecl::many2many("other_tags", "x.t"),
            ],
        ),
        model("x.t", vec![]),
    ])
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Many2many fields x.m.tags and x.m.other_tags use the same table and columns"
    );
    // siblings (explicit relation, same model and comodel) and mirrors are accepted: the fixtures
    // of `test_rel` (`relatives_ids` …, `categories` / `discussions`) build
    let registry = Harness::builder([&erp_test_models::test_rel::ADDON])
        .registry_only()
        .unwrap();
    assert!(registry.model("test_rel.active").is_some());
}

#[test]
fn spec_rel_034_many2many_set_null_is_refused() {
    let err = build(vec![
        model(
            "x.m",
            vec![FieldDecl::many2many("tags", "x.t").ondelete(OnDelete::SetNull)],
        ),
        model("x.t", vec![]),
    ])
    .unwrap_err();
    assert_eq!(
        err.message(),
        "The m2m field tags of model x.m declares its ondelete policy as being 'set null'. Only \
         'restrict' and 'cascade' make sense."
    );
}

#[test]
fn spec_rel_067_many2one_reference_needs_its_model_field() {
    let err = build(vec![model(
        "x.m",
        vec![FieldDecl::many2one_reference("res_id", "missing")],
    )])
    .unwrap_err();
    assert_eq!(
        err.message(),
        "Field x.m.res_id with unknown model_field 'missing'"
    );
}

#[test]
fn spec_model_020_inherits_exposes_parent_fields() {
    let registry = Harness::builder([&erp_test_models::test_rel::ADDON])
        .registry_only()
        .unwrap();
    let boxes = registry.model("test_rel.box").unwrap();
    let unit = registry.model("test_rel.unit").unwrap();
    for name in ["name", "state", "readonly_name", "line_ids"] {
        let f = boxes.field(name).unwrap();
        let inherited = f.inherited.expect(name);
        assert_eq!(inherited.parent, unit.id);
        assert_eq!(boxes.field_by_id(inherited.delegate).name, "unit_id");
        assert!(
            !f.store && !f.has_column(),
            "{name} has no storage of its own"
        );
        assert_eq!(f.readonly, unit.field(name).unwrap().readonly, "{name}");
        assert_eq!(f.copy, unit.field(name).unwrap().copy, "{name}");
    }
    assert!(
        boxes.field("size").unwrap().inherited.is_none(),
        "own field"
    );
    let unit_id = boxes.field("unit_id").unwrap();
    assert!(unit_id.delegate && unit_id.bypass_search_access);
    assert_eq!(boxes.inherits, [(unit.id, unit_id.id)]);
}

#[test]
fn spec_model_021_inherits_field_requirements() {
    let parent = || model("x.partner", vec![]);
    let child =
        |fields: Vec<FieldDecl>| model("x.child", fields).inherits(&[("x.partner", "partner_id")]);
    // (a) no field
    let err = build(vec![parent(), child(vec![])]).unwrap_err();
    assert!(
        err.message()
            .starts_with("Missing many2one field definition for _inherits reference 'partner_id'"),
        "{err}"
    );
    // (b) not required
    let err = build(vec![
        parent(),
        child(vec![FieldDecl::many2one("partner_id", "x.partner")]),
    ])
    .unwrap_err();
    let marked = "Field definition for _inherits reference 'partner_id' in 'x.child' must be \
                  marked as 'delegate', 'required' with ondelete='cascade' or 'restrict'";
    assert_eq!(err.message(), marked);
    // (c) required + set null: refused by the many2one rule first
    let err = build(vec![
        parent(),
        child(vec![
            FieldDecl::many2one("partner_id", "x.partner")
                .required(true)
                .ondelete(OnDelete::SetNull),
        ]),
    ])
    .unwrap_err();
    assert!(
        err.message()
            .contains("is required but declares its ondelete policy"),
        "{err}"
    );
    // (d) required without an explicit ondelete
    let err = build(vec![
        parent(),
        child(vec![
            FieldDecl::many2one("partner_id", "x.partner").required(true),
        ]),
    ])
    .unwrap_err();
    assert_eq!(err.message(), marked);
    // (e) valid: delegate and bypass_search_access
    let registry = build(vec![
        parent(),
        child(vec![
            FieldDecl::many2one("partner_id", "x.partner")
                .required(true)
                .ondelete(OnDelete::Cascade),
        ]),
    ])
    .unwrap();
    let f = registry
        .model("x.child")
        .unwrap()
        .field("partner_id")
        .unwrap();
    assert!(f.delegate && f.bypass_search_access);
}

#[test]
fn spec_model_022_transitive_delegation_fields() {
    let registry = Harness::builder([&erp_test_models::test_rel::ADDON])
        .registry_only()
        .unwrap();
    let pallet = registry.model("test_rel.pallet").unwrap();
    for name in [
        "name",
        "state",
        "field_in_box",
        "size",
        "field_in_pallet",
        "unit_id",
    ] {
        assert!(pallet.field(name).is_some(), "{name}");
    }
    assert!(pallet.field("field_in_pallet").unwrap().inherited.is_none());
    let name = pallet.field("name").unwrap().inherited.unwrap();
    assert_eq!(registry.model_by_id(name.parent).name, "test_rel.box");
    assert_eq!(pallet.field_by_id(name.delegate).name, "box_id");
    assert_eq!(
        pallet.rec_name.map(|f| pallet.field_by_id(f).name),
        Some("name"),
        "the rec name comes from the grand-parent"
    );
}

#[test]
fn spec_model_023_delegation_field_clash_last_parent_wins() {
    let parent_a = model("p.a", vec![FieldDecl::char("code")]);
    let parent_b = model("p.b", vec![FieldDecl::char("code")]);
    let child_ab = model(
        "x.child.ab",
        vec![
            FieldDecl::many2one("a_id", "p.a")
                .required(true)
                .ondelete(OnDelete::Cascade),
            FieldDecl::many2one("b_id", "p.b")
                .required(true)
                .ondelete(OnDelete::Cascade),
        ],
    )
    .inherits(&[("p.a", "a_id"), ("p.b", "b_id")]);

    let child_ba = model(
        "x.child.ba",
        vec![
            FieldDecl::many2one("a_id", "p.a")
                .required(true)
                .ondelete(OnDelete::Cascade),
            FieldDecl::many2one("b_id", "p.b")
                .required(true)
                .ondelete(OnDelete::Cascade),
        ],
    )
    .inherits(&[("p.b", "b_id"), ("p.a", "a_id")]);

    let registry = build(vec![parent_a, parent_b, child_ab, child_ba]).unwrap();

    // In child_ab, p.b is the last parent in _inherits: p.b wins.
    let ab = registry.model("x.child.ab").unwrap();
    let pb = registry.model("p.b").unwrap();
    let code_ab = ab.field("code").unwrap();
    let inherited_ab = code_ab.inherited.expect("code must be inherited");
    assert_eq!(inherited_ab.parent, pb.id, "p.b must win for child_ab");
    assert_eq!(ab.field_by_id(inherited_ab.delegate).name, "b_id");

    // In child_ba, p.a is the last parent in _inherits: p.a wins.
    let ba = registry.model("x.child.ba").unwrap();
    let pa = registry.model("p.a").unwrap();
    let code_ba = ba.field("code").unwrap();
    let inherited_ba = code_ba.inherited.expect("code must be inherited");
    assert_eq!(inherited_ba.parent, pa.id, "p.a must win for child_ba");
    assert_eq!(ba.field_by_id(inherited_ba.delegate).name, "a_id");
}

/// foreign key match)
#[tokio::test]
async fn spec_rel_008_023_033_relational_schema() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    // 008: many2one column, foreign key, index
    let col = rows(
        &mut tx,
        "SELECT data_type::text, is_nullable::text FROM information_schema.columns \
         WHERE table_name = 'test_rel_schema' AND column_name = 'many2one_id'",
    )
    .await;
    assert_eq!(col, vec![vec![Some("bigint".into()), Some("YES".into())]]);
    let fk = rows(
        &mut tx,
        "SELECT pg_get_constraintdef(k.oid)::text FROM pg_constraint k \
         JOIN pg_class c ON c.oid = k.conrelid WHERE c.relname = 'test_rel_schema' \
         AND k.contype = 'f' ORDER BY 1",
    )
    .await;
    assert!(
        fk.iter().any(|r| r[0].as_deref()
            == Some(
                "FOREIGN KEY (many2one_id) REFERENCES test_rel_mixed_rel(id) ON DELETE SET NULL"
            )),
        "{fk:?}"
    );
    let index = rows(
        &mut tx,
        "SELECT indexname::text FROM pg_indexes WHERE tablename = 'test_rel_schema' \
         AND indexdef LIKE '%(many2one_id)%'",
    )
    .await;
    assert_eq!(index.len(), 1, "many2one columns are indexed by default");
    // 023: no column for the one2many
    let cols = rows(
        &mut tx,
        "SELECT column_name::text FROM information_schema.columns \
         WHERE table_name = 'test_rel_schema' ORDER BY 1",
    )
    .await;
    let cols: Vec<String> = cols.into_iter().map(|r| r[0].clone().unwrap()).collect();
    assert!(!cols.contains(&"one2many_ids".to_owned()), "{cols:?}");
    assert!(!cols.contains(&"many2many_ids".to_owned()), "{cols:?}");
    // 033: the relation table
    let rel = "test_rel_mixed_rel_test_rel_schema_rel";
    let cols = rows(
        &mut tx,
        &format!(
            "SELECT column_name::text, data_type::text, is_nullable::text FROM \
             information_schema.columns WHERE table_name = '{rel}' ORDER BY 1"
        ),
    )
    .await;
    assert_eq!(
        cols,
        vec![
            vec![
                Some("test_rel_mixed_rel_id".into()),
                Some("bigint".into()),
                Some("NO".into())
            ],
            vec![
                Some("test_rel_schema_id".into()),
                Some("bigint".into()),
                Some("NO".into())
            ],
        ]
    );
    let kind = rows(
        &mut tx,
        &format!(
            "SELECT relkind::text, obj_description(oid, 'pg_class') FROM pg_class \
             WHERE relname = '{rel}'"
        ),
    )
    .await;
    assert_eq!(
        kind,
        vec![vec![
            Some("r".into()),
            Some("RELATION BETWEEN test_rel_schema AND test_rel_mixed_rel".into())
        ]]
    );
    let defs = rows(
        &mut tx,
        &format!(
            "SELECT pg_get_constraintdef(k.oid)::text FROM pg_constraint k JOIN pg_class c ON \
             c.oid = k.conrelid WHERE c.relname = '{rel}' AND k.contype IN ('f', 'p') ORDER BY 1"
        ),
    )
    .await;
    let defs: Vec<String> = defs.into_iter().map(|r| r[0].clone().unwrap()).collect();
    assert_eq!(
        defs,
        [
            "FOREIGN KEY (test_rel_mixed_rel_id) REFERENCES test_rel_mixed_rel(id) ON DELETE \
             CASCADE",
            "FOREIGN KEY (test_rel_schema_id) REFERENCES test_rel_schema(id) ON DELETE CASCADE",
            "PRIMARY KEY (test_rel_schema_id, test_rel_mixed_rel_id)",
        ]
    );
    let idx = rows(
        &mut tx,
        &format!(
            "SELECT indexdef::text FROM pg_indexes WHERE tablename = '{rel}' AND indexname NOT \
             LIKE '%pkey'"
        ),
    )
    .await;
    assert_eq!(idx.len(), 1);
    assert!(
        idx[0][0]
            .as_deref()
            .unwrap()
            .ends_with("(test_rel_mixed_rel_id, test_rel_schema_id)"),
        "{idx:?}"
    );
    // schema sync is idempotent
    tx.init_models(&[]).await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rel_012_foreign_key_kept_in_sync_with_ondelete() {
    let m_setnull = model(
        "x.rel_fk_m",
        vec![
            FieldDecl::many2one("target_id", "x.rel_fk_target").ondelete(OnDelete::SetNull),
            FieldDecl::many2one("comp_dep_id", "x.rel_fk_target").company_dependent(true),
            FieldDecl::many2one("abstract_id", "x.rel_fk_abstract"),
        ],
    );
    let target = || model("x.rel_fk_target", vec![]);
    let abstract_target = || model("x.rel_fk_abstract", vec![]).kind(ModelKind::Abstract);

    let h = Harness::builder([&T])
        .with_decls(vec![m_setnull, target(), abstract_target()])
        .build()
        .await
        .unwrap();

    let mut tx = h.begin().await.unwrap();
    let fks = rows(
        &mut tx,
        "SELECT pg_get_constraintdef(k.oid)::text FROM pg_constraint k \
         JOIN pg_class c ON c.oid = k.conrelid WHERE c.relname = 'x_rel_fk_m' \
         AND k.contype = 'f' \
         ORDER BY 1",
    )
    .await;
    let defs: Vec<String> = fks.into_iter().map(|r| r[0].clone().unwrap()).collect();
    assert_eq!(
        defs,
        vec![
            "FOREIGN KEY (create_uid) REFERENCES res_users(id) ON DELETE SET NULL",
            "FOREIGN KEY (target_id) REFERENCES x_rel_fk_target(id) ON DELETE SET NULL",
            "FOREIGN KEY (write_uid) REFERENCES res_users(id) ON DELETE SET NULL",
        ]
    );
    drop(tx);

    // Upgrade: ondelete changes to cascade
    let m_cascade = model(
        "x.rel_fk_m",
        vec![
            FieldDecl::many2one("target_id", "x.rel_fk_target").ondelete(OnDelete::Cascade),
            FieldDecl::many2one("comp_dep_id", "x.rel_fk_target").company_dependent(true),
            FieldDecl::many2one("abstract_id", "x.rel_fk_abstract"),
        ],
    );
    let reg2 = build(vec![m_cascade, target(), abstract_target()]).unwrap();

    let orm2 = erp_orm::Orm::builder(reg2, h.db().pool().clone())
        .policy(erp_orm::StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let report = orm2.sync_schema().await.unwrap();
    assert!(report.is_empty());

    let mut tx = orm2.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let fks2 = rows(
        &mut tx,
        "SELECT pg_get_constraintdef(k.oid)::text FROM pg_constraint k \
         JOIN pg_class c ON c.oid = k.conrelid WHERE c.relname = 'x_rel_fk_m' \
         AND k.contype = 'f' \
         ORDER BY 1",
    )
    .await;
    let defs2: Vec<String> = fks2.into_iter().map(|r| r[0].clone().unwrap()).collect();
    assert_eq!(
        defs2,
        vec![
            "FOREIGN KEY (create_uid) REFERENCES res_users(id) ON DELETE SET NULL",
            "FOREIGN KEY (target_id) REFERENCES x_rel_fk_target(id) ON DELETE CASCADE",
            "FOREIGN KEY (write_uid) REFERENCES res_users(id) ON DELETE SET NULL",
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
