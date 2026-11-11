//! Registry composition cases from the MODEL and FIELD specs. Pure: no database.
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use erp_fields::{ColumnType, Digits, ErrorKind, FieldType, IndexKind, OnDelete};
use erp_registry::{
    AddonDescriptor, FieldDecl, FieldDefault, KindDecl, ModelDecl, ModelKind, Registry,
    RegistryBuilder, SelectionOnDeleteDecl, SignalTriggerDecl, StaticValue, TableObjectDecl,
    TableObjectKind, TriggerSignal,
};

static MOTHER_BASE: AddonDescriptor = AddonDescriptor {
    name: "mother_base",
    depends: &["base"],
    exact_only: false,
};
static MOTHER_1: AddonDescriptor = AddonDescriptor {
    name: "mother_inherit_1",
    depends: &["mother_base"],
    exact_only: false,
};
static MOTHER_2: AddonDescriptor = AddonDescriptor {
    name: "mother_inherit_2",
    depends: &["mother_inherit_1"],
    exact_only: false,
};
static MOTHER_3: AddonDescriptor = AddonDescriptor {
    name: "mother_inherit_3",
    depends: &["mother_inherit_2"],
    exact_only: false,
};
static MOTHER_4: AddonDescriptor = AddonDescriptor {
    name: "mother_inherit_4",
    depends: &["mother_inherit_3"],
    exact_only: false,
};
static OTHER: AddonDescriptor = AddonDescriptor {
    name: "other",
    depends: &["base"],
    exact_only: false,
};

const MOTHER: &[&str] = &["test.inherit.mother"];

static MOTHER_DECLS: [ModelDecl; 5] = [
    ModelDecl::new("mother_base", "test.inherit.mother")
        .description("Test Inherit Mother")
        .fields(
            const {
                &[
                    FieldDecl::char("name").default(StaticValue::Str("Foo")),
                    FieldDecl::char("surname").compute("_compute_surname"),
                ]
            },
        ),
    ModelDecl::extension("mother_inherit_1", "test.inherit.mother", MOTHER).fields(
        const {
            &[
                FieldDecl::char("field_in_mother"),
                FieldDecl::many2one("partner_id", "test.inherit.partner"),
                FieldDecl::selection("state", &[("a", "A"), ("b", "B")])
                    .default(StaticValue::Str("a")),
                FieldDecl::char("name")
                    .required(true)
                    .default(StaticValue::Str("Bar")),
            ]
        },
    ),
    ModelDecl::extension("mother_inherit_2", "test.inherit.mother", MOTHER).fields(
        const {
            &[
                FieldDecl::selection_add("state", &[("c", Some("C"))]).no_default(),
                FieldDecl::char("field_in_mother_2"),
            ]
        },
    ),
    ModelDecl::extension("mother_inherit_3", "test.inherit.mother", MOTHER).fields(
        const {
            &[
                FieldDecl::selection_add("state", &[("d", Some("D")), ("b", None)]),
                FieldDecl::char("field_in_mother_3"),
            ]
        },
    ),
    ModelDecl::extension("mother_inherit_4", "test.inherit.mother", MOTHER).fields(
        const {
            &[
                FieldDecl::selection_add("state", &[("e", Some("E"))]),
                FieldDecl::char("field_in_mother_4"),
            ]
        },
    ),
];

static PARTNER_DECL: ModelDecl = ModelDecl::new("mother_base", "test.inherit.partner")
    .description("Test Inherit Partner")
    .fields(const { &[FieldDecl::char("name")] });

fn mothers(addons: &[&'static AddonDescriptor]) -> Registry {
    RegistryBuilder::new(addons.iter().copied())
        .with_decls(
            MOTHER_DECLS
                .iter()
                .filter(|d| addons.iter().any(|a| a.name == d.addon)),
        )
        .with_decls([&PARTNER_DECL])
        .build()
        .unwrap()
}

fn all_mothers() -> Registry {
    mothers(&[&MOTHER_BASE, &MOTHER_1, &MOTHER_2, &MOTHER_3, &MOTHER_4])
}

fn build(
    addons: &[&'static AddonDescriptor],
    decls: Vec<ModelDecl>,
) -> erp_fields::Result<Registry> {
    let decls: Vec<&'static ModelDecl> = decls
        .into_iter()
        .map(|d| &*Box::leak(Box::new(d)))
        .collect();
    RegistryBuilder::new(addons.iter().copied())
        .with_decls(decls)
        .build()
}

fn field_names(registry: &Registry, model: &str) -> Vec<&'static str> {
    let mut names: Vec<_> = registry
        .model(model)
        .unwrap()
        .fields
        .iter()
        .map(|f| f.name)
        .collect();
    names.sort_unstable();
    names
}

#[test]
fn spec_model_001_defining_a_new_model() {
    let registry = mothers(&[&MOTHER_BASE]);
    let mother = registry.model("test.inherit.mother").unwrap();
    assert_eq!(mother.table, "test_inherit_mother");
    assert_eq!(mother.description, "Test Inherit Mother");
    for f in [
        "id",
        "display_name",
        "create_uid",
        "create_date",
        "write_uid",
        "write_date",
        "name",
    ] {
        assert!(mother.field(f).is_some(), "missing {f}");
    }
    assert_eq!(mother.rec_name, mother.field_id("name"));
}

#[test]
fn spec_model_002_extension_order() {
    let registry = all_mothers();
    let mother = registry.model("test.inherit.mother").unwrap();
    let mro: Vec<_> = mother.mro.iter().map(|d| (d.name, d.addon)).collect();
    assert_eq!(
        mro,
        [
            ("test.inherit.mother", "mother_inherit_4"),
            ("test.inherit.mother", "mother_inherit_3"),
            ("test.inherit.mother", "mother_inherit_2"),
            ("test.inherit.mother", "mother_inherit_1"),
            ("test.inherit.mother", "mother_base"),
            // the always-installed `web` addon extends `base`
            ("base", "web"),
            ("base", "base"),
        ]
    );
    assert!(mother.field("field_in_mother").is_some());
    assert!(mother.field("field_in_mother_4").is_some());
}

#[test]
fn spec_model_003_inherit_string_implies_name() {
    let registry = mothers(&[&MOTHER_BASE, &MOTHER_1]);
    let names: Vec<_> = registry.models().iter().map(|m| m.name).collect();
    assert_eq!(
        names,
        [
            "base",
            "res.users",
            "test.inherit.mother",
            "test.inherit.partner"
        ]
    );
    assert!(MOTHER_DECLS[1].is_extension());
}

#[test]
fn spec_model_004_extending_a_missing_model() {
    let err = build(
        &[&OTHER],
        vec![ModelDecl::extension(
            "other",
            "no.such.model",
            &["no.such.model"],
        )],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Model 'no.such.model' does not exist in registry."
    );
}

/// A currency definition must obey the same explicit-inheritance rule as every model.
#[test]
fn duplicate_currency_definition_requires_explicit_inheritance() {
    static TEST_ORM: AddonDescriptor = AddonDescriptor {
        name: "test_orm",
        depends: &["base"],
        exact_only: false,
    };
    for addon in ["base", "test_orm"] {
        let err = build(
            &[&TEST_ORM],
            vec![
                ModelDecl::new("base", "res.currency"),
                ModelDecl::new(addon, "res.currency"),
            ],
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "model definition 'res.currency' of addon '{addon}' defines model \
                 'res.currency', which already exists; extend it with inherit = ['res.currency']"
            )
        );
    }
}

#[test]
fn spec_model_005_new_model_with_missing_parent() {
    let err = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.child").inherit(&["no.such.parent"])],
    )
    .unwrap_err();
    assert_eq!(
        err.message(),
        "Model 'x.child' inherits from non-existing model 'no.such.parent'."
    );
}

#[test]
fn spec_model_006_prototype_inheritance_sees_later_parent_extensions() {
    static PARENT_EXT: AddonDescriptor = AddonDescriptor {
        name: "parent_ext",
        depends: &["other"],
        exact_only: false,
    };
    let registry = build(
        &[&OTHER, &PARENT_EXT],
        vec![
            ModelDecl::new("other", "test_inherit_parent").kind(ModelKind::Abstract),
            ModelDecl::new("other", "test_inherit_child")
                .inherit(&["test_inherit_parent"])
                .fields(const { &[FieldDecl::integer("bar")] })
                .at("child.rs", 2),
            ModelDecl::extension(
                "parent_ext",
                "test_inherit_parent",
                &["test_inherit_parent"],
            )
            .kind(ModelKind::Abstract)
            .fields(const { &[FieldDecl::integer("foo")] })
            .table_objects(
                const {
                    &[TableObjectDecl {
                        name: "_unique_foo",
                        kind: TableObjectKind::Constraint {
                            definition: "UNIQUE(foo)",
                            message: "foo must be unique",
                        },
                    }]
                },
            ),
        ],
    )
    .unwrap();
    let parent = registry.model("test_inherit_parent").unwrap();
    let child = registry.model("test_inherit_child").unwrap();
    assert!(parent.field("foo").is_some());
    assert!(parent.field("bar").is_none());
    assert!(child.field("foo").is_some() && child.field("bar").is_some());
    assert_eq!(parent.table, "test_inherit_parent");
    assert_eq!(child.table, "test_inherit_child");
    assert_eq!(parent.table_objects.len(), 1);
    assert_eq!(child.table_objects.len(), 1);
    assert_eq!(
        child.table_objects[0].full_name,
        "test_inherit_child_unique_foo"
    );
    assert_eq!(child.inherit, ["test_inherit_parent"]);
    // the child's MRO reaches the parent's later extension
    let addons: Vec<_> = child.mro.iter().map(|d| d.addon).collect();
    assert_eq!(addons, ["other", "parent_ext", "other", "web", "base"]);
}

#[test]
fn spec_model_007_every_model_inherits_base() {
    let registry = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.thing"),
            ModelDecl::extension("other", "base", &["base"])
                .kind(ModelKind::Abstract)
                .fields(const { &[FieldDecl::char("x_note_like")] })
                .at("z.rs", 1),
        ],
    )
    .unwrap();
    assert!(
        registry
            .model("x.thing")
            .unwrap()
            .field("x_note_like")
            .is_some()
    );
    let base = registry.model("base").unwrap();
    assert!(base.inherit.is_empty());
    // this extension, the `web` addon's (always installed) and base's own
    assert_eq!(base.mro.len(), 3);
}

#[test]
fn spec_model_008_extension_may_not_change_table() {
    let err = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "res.partner"),
            ModelDecl::extension("other", "res.partner", &["res.partner"])
                .table("other")
                .at("z.rs", 9),
        ],
    )
    .unwrap_err();
    assert!(
        err.message()
            .contains("cannot redefine _table while extending model 'res.partner'"),
        "{err}"
    );
}

#[test]
fn spec_model_009_extension_may_not_change_kind() {
    let cases = [
        (
            ModelKind::Abstract,
            ModelKind::Regular,
            "transforms the abstract model",
        ),
        (
            ModelKind::Regular,
            ModelKind::Transient,
            "into a transient model",
        ),
        (
            ModelKind::Transient,
            ModelKind::Regular,
            "transforms the transient model",
        ),
    ];
    for (base_kind, ext_kind, expected) in cases {
        let err = build(
            &[&OTHER],
            vec![
                ModelDecl::new("other", "x.m").kind(base_kind),
                ModelDecl::extension("other", "x.m", &["x.m"])
                    .kind(ext_kind)
                    .at("z.rs", 1),
            ],
        )
        .unwrap_err();
        assert!(err.message().contains(expected), "{err}");
    }
}

#[test]
fn spec_model_010_abstract_cannot_inherit_concrete() {
    let err = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "res.partner"),
            ModelDecl::new("other", "x.mixin")
                .kind(ModelKind::Abstract)
                .inherit(&["res.partner"])
                .at("z.rs", 1),
        ],
    )
    .unwrap_err();
    assert!(
        err.message().contains(
            "abstract model 'x.mixin' cannot inherit from non-abstract model 'res.partner'"
        ),
        "{err}"
    );
}

#[test]
fn spec_model_011_magic_fields_by_model_kind() {
    let registry = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.regular"),
            ModelDecl::new("other", "x.nolog").log_access(false),
            ModelDecl::new("other", "x.abstract").kind(ModelKind::Abstract),
            ModelDecl::new("other", "x.transient").kind(ModelKind::Transient),
            ModelDecl::new("other", "x.own_date")
                .fields(const { &[FieldDecl::date("create_date").string("Mine")] }),
        ],
    )
    .unwrap();
    let magic = [
        "create_date",
        "create_uid",
        "display_name",
        "id",
        "write_date",
        "write_uid",
    ];
    assert_eq!(field_names(&registry, "x.regular"), magic);
    assert_eq!(field_names(&registry, "x.transient"), magic);
    assert_eq!(field_names(&registry, "x.nolog"), ["display_name", "id"]);
    assert_eq!(field_names(&registry, "x.abstract"), ["display_name", "id"]);
    assert!(!registry.model("x.abstract").unwrap().has_table());
    let regular = registry.model("x.regular").unwrap();
    let uid = regular.field("create_uid").unwrap();
    assert_eq!(
        uid.ty,
        FieldType::Many2one {
            comodel: "res.users".into(),
            ondelete: OnDelete::SetNull
        }
    );
    assert_eq!(uid.column(), Some(ColumnType::Int8));
    assert_eq!(uid.index, None, "audit many2one fields are not indexed");
    let own = registry
        .model("x.own_date")
        .unwrap()
        .field("create_date")
        .unwrap();
    assert_eq!(own.ty, FieldType::Date);
    assert_eq!(own.string, "Mine");
}

#[test]
fn spec_model_012_transient_requires_log_access() {
    let err = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.wizard")
                .kind(ModelKind::Transient)
                .log_access(false),
        ],
    )
    .unwrap_err();
    assert_eq!(
        err.message(),
        "TransientModels must have log_access turned on, in order to implement their vacuum policy"
    );
}

#[test]
fn spec_model_013_description_defaults_to_name() {
    let registry = build(&[&OTHER], vec![ModelDecl::new("other", "foo.bar")]).unwrap();
    assert_eq!(registry.model("foo.bar").unwrap().description, "foo.bar");
}

#[test]
fn spec_model_014_table_name_validation() {
    let ok = build(&[&OTHER], vec![ModelDecl::new("other", "x.ok")]).unwrap();
    assert_eq!(ok.model("x.ok").unwrap().table, "x_ok");
    let err = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.bad").table("bad-name")],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "Invalid characters in table name 'bad-name'");
    let long_name: &'static str = Box::leak(format!("x.{}", "a".repeat(62)).into_boxed_str());
    let err = build(&[&OTHER], vec![ModelDecl::new("other", long_name)]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(err.message().ends_with("is too long"), "{err}");
}

#[test]
fn spec_model_015_field_set_of_a_model() {
    static OVERLAY: AddonDescriptor = AddonDescriptor {
        name: "db_overlay",
        depends: &["mother_inherit_4"],
        exact_only: false,
    };
    let mut addons: Vec<&'static AddonDescriptor> = vec![
        &MOTHER_BASE,
        &MOTHER_1,
        &MOTHER_2,
        &MOTHER_3,
        &MOTHER_4,
        &OVERLAY,
    ];
    let overlay: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::extension("db_overlay", "test.inherit.mother", MOTHER)
            .fields(const { &[FieldDecl::char("x_foo")] }),
    ));
    let registry = RegistryBuilder::new(addons.iter().copied())
        .with_decls(MOTHER_DECLS.iter())
        .with_decls([&PARTNER_DECL, overlay])
        .build()
        .unwrap();
    let names = field_names(&registry, "test.inherit.mother");
    for f in [
        "name",
        "surname",
        "field_in_mother",
        "partner_id",
        "state",
        "field_in_mother_4",
        "x_foo",
        "create_uid",
        "id",
    ] {
        assert!(names.contains(&f), "missing {f}");
    }
    let name = registry
        .model("test.inherit.mother")
        .unwrap()
        .field("name")
        .unwrap();
    assert_eq!(name.modules, ["mother_base", "mother_inherit_1"]);
    assert_eq!(names.iter().filter(|n| **n == "name").count(), 1);

    // reloading without the addon that added `state` removes it
    addons.retain(|a| a.name == "mother_base");
    let registry = mothers(&addons);
    assert!(
        registry
            .model("test.inherit.mother")
            .unwrap()
            .field("state")
            .is_none()
    );
}

#[test]
fn inherits_access_defaults_to_true_and_the_newest_declaration_wins() {
    const DELEGATE: &[FieldDecl] = &[FieldDecl::many2one("parent_id", "x.parent")
        .required(true)
        .ondelete(OnDelete::Cascade)];
    let registry = build(
        &[&MOTHER_BASE, &MOTHER_1],
        vec![
            ModelDecl::new("mother_base", "x.parent"),
            ModelDecl::new("mother_base", "x.child")
                .inherits(&[("x.parent", "parent_id")])
                .fields(DELEGATE),
            ModelDecl::new("mother_base", "x.free")
                .inherits(&[("x.parent", "parent_id")])
                .inherits_access(false)
                .fields(DELEGATE),
            ModelDecl::extension("mother_inherit_1", "x.free", &["x.free"]).inherits_access(true),
            ModelDecl::extension("mother_inherit_1", "x.child", &["x.child"])
                .inherits_access(false),
        ],
    )
    .unwrap();
    let inherits_access = |m: &str| registry.model(m).unwrap().inherits_access;
    assert!(inherits_access("x.parent"));
    assert!(!inherits_access("x.child"), "the extension says false");
    assert!(inherits_access("x.free"), "the extension says true again");
    let registry = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.parent"),
            ModelDecl::new("other", "x.free")
                .inherits(&[("x.parent", "parent_id")])
                .inherits_access(false)
                .fields(DELEGATE),
        ],
    )
    .unwrap();
    assert!(!registry.model("x.free").unwrap().inherits_access);
}

#[test]
fn spec_model_016_rec_name_resolution() {
    let registry = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.a").fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("other", "x.b").fields(const { &[FieldDecl::char("code")] }),
            ModelDecl::new("other", "x.c")
                .rec_name("code")
                .fields(const { &[FieldDecl::char("code")] }),
        ],
    )
    .unwrap();
    let rec = |m: &str| {
        let model = registry.model(m).unwrap();
        model.rec_name.map(|id| model.field_by_id(id).name)
    };
    assert_eq!(rec("x.a"), Some("name"));
    assert_eq!(rec("x.b"), None);
    assert_eq!(rec("x.c"), Some("code"));
    let err = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.d").rec_name("missing")],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "Invalid _rec_name='missing' for model 'x.d'");
}

#[test]
fn spec_model_017_active_name_resolution() {
    let registry = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.a").fields(const { &[FieldDecl::boolean("active")] }),
            ModelDecl::new("other", "x.b").fields(const { &[FieldDecl::boolean("x_active")] }),
            ModelDecl::new("other", "x.d"),
        ],
    )
    .unwrap();
    let active = |m: &str| {
        let model = registry.model(m).unwrap();
        model.active_name.map(|id| model.field_by_id(id).name)
    };
    assert_eq!(active("x.a"), Some("active"));
    assert_eq!(active("x.b"), Some("x_active"));
    assert_eq!(active("x.d"), None);
    let err = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.c")
                .active_name("enabled")
                .fields(const { &[FieldDecl::boolean("enabled")] }),
        ],
    )
    .unwrap_err();
    assert!(
        err.message()
            .contains("only 'active' and 'x_active' are supported"),
        "{err}"
    );
}

#[test]
fn spec_field_001_default_labels_in_registry() {
    let registry = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.labels").fields(
            const {
                &[
                    FieldDecl::char("customer_ref"),
                    FieldDecl::integer("partner_id"),
                    FieldDecl::char("tag_ids"),
                    FieldDecl::char("iban2code"),
                    FieldDecl::char("name"),
                ]
            },
        )],
    )
    .unwrap();
    let model = registry.model("x.labels").unwrap();
    let label = |f: &str| model.field(f).unwrap().string.clone();
    assert_eq!(label("customer_ref"), "Customer Ref");
    assert_eq!(label("partner_id"), "Partner");
    assert_eq!(label("tag_ids"), "Tag");
    assert_eq!(label("iban2code"), "Iban2Code");
    assert_eq!(label("name"), "Name");
    assert_eq!(model.field("display_name").unwrap().string, "Display Name");
    assert_eq!(model.field("id").unwrap().string, "ID");
}

#[test]
fn spec_field_002_defaults_and_default_removal() {
    let registry = all_mothers();
    let mother = registry.model("test.inherit.mother").unwrap();
    assert!(matches!(
        mother.field("name").unwrap().default,
        Some(FieldDefault::Value(StaticValue::Str("Bar")))
    ));
    assert!(mother.field("state").unwrap().default.is_none());
}

#[test]
fn spec_field_003_attribute_merge() {
    static DAUGHTER: AddonDescriptor = AddonDescriptor {
        name: "daughter",
        depends: &["mother_inherit_1"],
        exact_only: false,
    };
    let daughter: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("daughter", "test.inherit.daughter")
            .inherit(MOTHER)
            .fields(const { &[FieldDecl::char("name").default(StaticValue::Str("Baz"))] }),
    ));
    let registry = RegistryBuilder::new([&MOTHER_BASE, &MOTHER_1, &DAUGHTER])
        .with_decls(MOTHER_DECLS[..2].iter())
        .with_decls([&PARTNER_DECL, daughter])
        .build()
        .unwrap();
    let mother = registry
        .model("test.inherit.mother")
        .unwrap()
        .field("name")
        .unwrap();
    assert!(mother.required);
    assert!(matches!(
        mother.default,
        Some(FieldDefault::Value(StaticValue::Str("Bar")))
    ));
    let child = registry
        .model("test.inherit.daughter")
        .unwrap()
        .field("name")
        .unwrap();
    // prototype inheritance: the new model inherits the merged definition, then overrides
    assert!(matches!(
        child.default,
        Some(FieldDefault::Value(StaticValue::Str("Baz")))
    ));
    assert_eq!(
        child.modules,
        ["mother_base", "mother_inherit_1", "daughter"]
    );
}

#[test]
fn spec_field_004_type_change_discards_parameters() {
    let registry = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.note").fields(
                const {
                    &[FieldDecl::char("note")
                        .string("Note")
                        .required(true)
                        .size(10)]
                },
            ),
            ModelDecl::extension("other", "x.note", &["x.note"])
                .fields(const { &[FieldDecl::text("note")] })
                .at("z.rs", 1),
        ],
    )
    .unwrap();
    let note = registry.model("x.note").unwrap().field("note").unwrap();
    assert_eq!(note.ty, FieldType::Text);
    assert!(!note.required);
    assert_eq!(note.string, "Note");
    assert_eq!(note.column(), Some(ColumnType::Text));
}

#[test]
fn spec_field_006_computed_attribute_defaults() {
    let registry = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.comp").fields(
            const {
                &[
                    FieldDecl::char("plain_compute").compute("_c1"),
                    FieldDecl::char("stored_compute").compute("_c2").store(true),
                    FieldDecl::char("stored_editable")
                        .compute("_c3")
                        .store(true)
                        .readonly(false),
                    FieldDecl::char("with_inverse")
                        .compute("_c4")
                        .inverse("_i4"),
                ]
            },
        )],
    )
    .unwrap();
    let model = registry.model("x.comp").unwrap();
    let f = |n: &str| model.field(n).unwrap().clone();
    let plain = f("plain_compute");
    assert!(!plain.store && !plain.compute_sudo() && plain.readonly && !plain.copy);
    assert!(plain.column().is_none() && !plain.prefetch);
    let stored = f("stored_compute");
    assert!(stored.store && stored.compute_sudo() && stored.readonly && !stored.copy);
    let editable = f("stored_editable");
    assert!(!editable.readonly && editable.copy);
    assert!(!f("with_inverse").readonly);
    let display = model.field("display_name").unwrap();
    assert!(!display.store && display.readonly);
}

#[test]
fn spec_field_025_monetary_currency_field_resolution() {
    let ok = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "res.currency"),
            ModelDecl::new("other", "x.money").fields(
                const {
                    &[
                        FieldDecl::new(
                            "amount",
                            KindDecl::Monetary {
                                currency_field: Some("base_currency_id"),
                            },
                        ),
                        FieldDecl::new(
                            "other_amount",
                            KindDecl::Monetary {
                                currency_field: None,
                            },
                        ),
                        FieldDecl::many2one("base_currency_id", "res.currency"),
                        FieldDecl::many2one("currency_id", "res.currency"),
                    ]
                },
            ),
        ],
    )
    .unwrap();
    let model = ok.model("x.money").unwrap();
    let currency = |f: &str| match &model.field(f).unwrap().ty {
        FieldType::Monetary { currency_field } => currency_field.clone(),
        _ => None,
    };
    assert_eq!(currency("amount").as_deref(), Some("base_currency_id"));
    assert_eq!(currency("other_amount").as_deref(), Some("currency_id"));
    let err = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.nomoney").fields(
            const {
                &[FieldDecl::new(
                    "amount",
                    KindDecl::Monetary {
                        currency_field: None,
                    },
                )]
            },
        )],
    )
    .unwrap_err();
    assert_eq!(
        err.message(),
        "Field x.nomoney.amount with unknown currency_field None"
    );
}

#[test]
fn spec_field_033_selection_add_ordering() {
    let registry = all_mothers();
    let state = registry
        .model("test.inherit.mother")
        .unwrap()
        .field("state")
        .unwrap();
    let FieldType::Selection {
        values: Some(values),
        ..
    } = &state.ty
    else {
        panic!("static selection expected: {:?}", state.ty);
    };
    let keys: Vec<_> = values.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["a", "d", "b", "c", "e"]);
    assert_eq!(values[2], ("b".to_owned(), "B".to_owned()));
}

#[test]
fn spec_field_034_ondelete_validation_at_setup() {
    static BASE: AddonDescriptor = AddonDescriptor {
        name: "test_sel_base",
        depends: &["base"],
        exact_only: false,
    };
    static EXT: AddonDescriptor = AddonDescriptor {
        name: "test_sel_ext",
        depends: &["test_sel_base"],
        exact_only: false,
    };

    // 1. Unknown / invalid ondelete policy -> ValueError
    let err =
        SelectionOnDeleteDecl::parse_for_field("m.req", "state", "random", "poop").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains(
            "ondelete policy \"poop\" for selection value \"random\" is not a valid ondelete policy"
        ),
        "actual: {}",
        err.message()
    );
    assert_eq!(
        erp_fields::SelectionOnDelete::parse("set null"),
        Ok(erp_fields::SelectionOnDelete::SetNull)
    );
    assert_eq!(
        erp_fields::SelectionOnDelete::parse("cascade"),
        Ok(erp_fields::SelectionOnDelete::Cascade)
    );
    assert_eq!(
        erp_fields::SelectionOnDelete::parse("set default"),
        Ok(erp_fields::SelectionOnDelete::SetDefault)
    );
    assert_eq!(
        erp_fields::SelectionOnDelete::parse("set foo"),
        Ok(erp_fields::SelectionOnDelete::SetValue("foo".into()))
    );
    assert!(erp_fields::SelectionOnDelete::parse("poop").is_err());

    // 2. 'set default' on a field without default -> AssertionError
    let err = build(
        &[&BASE, &EXT],
        vec![
            ModelDecl::new("test_sel_base", "m.opt")
                .fields(const { &[FieldDecl::selection("state", &[("foo", "Foo")])] }),
            ModelDecl::extension("test_sel_ext", "m.opt", &["m.opt"]).fields(
                const {
                    &[
                        FieldDecl::selection_add("state", &[("corona", Some("Corona"))])
                            .selection_ondelete(&[("corona", SelectionOnDeleteDecl::SetDefault)]),
                    ]
                },
            ),
        ],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains(
            "ondelete policy of type 'set default' is invalid for this field as it does not define a default!"
        ),
        "actual: {}",
        err.message()
    );

    // 3. 'set foooo' where 'foooo' is not a valid selection value -> AssertionError
    let err =
        build(
            &[&BASE, &EXT],
            vec![
                ModelDecl::new("test_sel_base", "m.opt2")
                    .fields(const { &[FieldDecl::selection("state", &[("foo", "Foo")])] }),
                ModelDecl::extension("test_sel_ext", "m.opt2", &["m.opt2"]).fields(
                    const {
                        &[FieldDecl::selection_add(
                            "state",
                            &[("westvleteren", Some("Westvleteren"))],
                        )
                        .selection_ondelete(&[(
                            "westvleteren",
                            SelectionOnDeleteDecl::SetValue("foooo"),
                        )])]
                    },
                ),
            ],
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains(
            "ondelete policy of type 'set %' must be either 'set null', 'set default', or 'set value' where value is a valid selection value."
        ),
        "actual: {}",
        err.message()
    );

    // 4. 'set null' on a required field -> ValueError
    let err = build(
        &[&BASE, &EXT],
        vec![
            ModelDecl::new("test_sel_base", "m.req2").fields(
                const {
                    &[FieldDecl::selection("state", &[("foo", "Foo")])
                        .required(true)
                        .default(StaticValue::Str("foo"))]
                },
            ),
            ModelDecl::extension("test_sel_ext", "m.req2", &["m.req2"]).fields(
                const {
                    &[FieldDecl::selection_add("state", &[("brap", Some("Brap"))])
                        .selection_ondelete(&[("brap", SelectionOnDeleteDecl::SetNull)])]
                },
            ),
        ],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains(
            "required selection fields must define an ondelete policy that implements the proper cleanup of the corresponding records upon module uninstallation."
        ),
        "actual: {}",
        err.message()
    );

    // 5. no ondelete on a required field (implicit 'set null') -> ValueError
    let err = build(
        &[&BASE, &EXT],
        vec![
            ModelDecl::new("test_sel_base", "m.req3").fields(
                const {
                    &[FieldDecl::selection("state", &[("foo", "Foo")])
                        .required(true)
                        .default(StaticValue::Str("foo"))]
                },
            ),
            ModelDecl::extension("test_sel_ext", "m.req3", &["m.req3"])
                .fields(const { &[FieldDecl::selection_add("state", &[("brap", Some("Brap"))])] }),
        ],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains(
            "required selection fields must define an ondelete policy that implements the proper cleanup of the corresponding records upon module uninstallation."
        ),
        "actual: {}",
        err.message()
    );

    // 6. Valid cases build successfully
    let reg = build(
        &[&BASE, &EXT],
        vec![
            ModelDecl::new("test_sel_base", "m.valid").fields(
                const {
                    &[
                        FieldDecl::selection("state", &[("foo", "Foo"), ("bar", "Bar")])
                            .required(true)
                            .default(StaticValue::Str("foo")),
                    ]
                },
            ),
            ModelDecl::extension("test_sel_ext", "m.valid", &["m.valid"]).fields(
                const {
                    &[FieldDecl::selection_add(
                        "state",
                        &[
                            ("baz", Some("Baz")),
                            ("eggs", Some("Eggs")),
                            ("bacon", Some("Bacon")),
                            ("cb", Some("Callback")),
                        ],
                    )
                    .selection_ondelete(&[
                        ("baz", SelectionOnDeleteDecl::SetDefault),
                        ("eggs", SelectionOnDeleteDecl::Cascade),
                        ("bacon", SelectionOnDeleteDecl::SetValue("bar")),
                        ("cb", SelectionOnDeleteDecl::Callback("my_callback")),
                    ])]
                },
            ),
        ],
    )
    .unwrap();
    assert!(reg.model("m.valid").is_some());
}

#[test]
fn spec_field_040_column_types() {
    let registry = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.mixed").fields(
            const {
                &[
                    FieldDecl::boolean("boolean"),
                    FieldDecl::integer("integer"),
                    FieldDecl::float("float"),
                    FieldDecl::float_digits(
                        "number",
                        Digits::Fixed {
                            precision: 0,
                            scale: 2,
                        },
                    ),
                    FieldDecl::char("char"),
                    FieldDecl::char("size").size(3),
                    FieldDecl::text("text"),
                    FieldDecl::html("html"),
                    FieldDecl::date("date"),
                    FieldDecl::datetime("moment"),
                    FieldDecl::json("json"),
                ]
            },
        )],
    )
    .unwrap();
    let model = registry.model("x.mixed").unwrap();
    let col = |f: &str| model.field(f).unwrap().column().map(ColumnType::sql);
    assert_eq!(col("boolean").as_deref(), Some("bool"));
    assert_eq!(col("integer").as_deref(), Some("int4"));
    assert_eq!(col("float").as_deref(), Some("float8"));
    assert_eq!(col("number").as_deref(), Some("numeric"));
    assert_eq!(col("char").as_deref(), Some("varchar"));
    assert_eq!(col("size").as_deref(), Some("varchar(3)"));
    assert_eq!(col("text").as_deref(), Some("text"));
    assert_eq!(col("html").as_deref(), Some("text"));
    assert_eq!(col("date").as_deref(), Some("date"));
    assert_eq!(col("moment").as_deref(), Some("timestamp"));
    assert_eq!(col("json").as_deref(), Some("jsonb"));
    assert_eq!(col("id"), None);
}

#[test]
fn many2one_is_indexed_by_default() {
    let registry = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.link").fields(
            const {
                &[
                    FieldDecl::many2one("partner_id", "x.link"),
                    FieldDecl::many2one("free_id", "x.link").index(None),
                ]
            },
        )],
    )
    .unwrap();
    let model = registry.model("x.link").unwrap();
    assert_eq!(
        model.field("partner_id").unwrap().index,
        Some(IndexKind::Btree)
    );
    assert_eq!(model.field("free_id").unwrap().index, None);
}

#[test]
fn table_objects_need_a_leading_underscore() {
    let err = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.t").table_objects(
            const {
                &[TableObjectDecl {
                    name: "positive",
                    kind: TableObjectKind::Constraint {
                        definition: "CHECK(1 = 1)",
                        message: "",
                    },
                }]
            },
        )],
    )
    .unwrap_err();
    assert!(
        err.message()
            .starts_with("Names of SQL objects in a model must start with '_'")
    );
}

const fn check_object(name: &'static str) -> TableObjectDecl {
    TableObjectDecl {
        name,
        kind: TableObjectKind::Constraint {
            definition: "CHECK(1 = 1)",
            message: "",
        },
    }
}

const fn exclusion_object(name: &'static str) -> TableObjectDecl {
    TableObjectDecl {
        name,
        kind: TableObjectKind::Exclusion(erp_registry::ExclusionDecl::new(
            ("a", "b"),
            erp_fields::RangeBounds::HalfOpen,
            "taken",
        )),
    }
}

/// Review S1: redeclaring an object under the same full name with another kind is refused.
#[test]
fn redeclaring_an_object_with_another_kind_is_refused() {
    let native = ModelDecl::new("other", "x.t")
        .fields(const { &[FieldDecl::date("a"), FieldDecl::date("b")] })
        .table_objects(const { &[check_object("_guard")] })
        .at("models.rs", 3);
    let mut runtime = ModelDecl::extension("other", "x.t", &["x.t"])
        .table_objects(Box::leak(Box::new([exclusion_object("_guard")])))
        .at("models/guard.toml", 7);
    runtime.runtime = true;
    let err = build(&[&OTHER], vec![native, runtime]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "SQL object 'x_t_guard' of model 'x.t' declared by addon 'other' (models/guard.toml:7) \
         has a different kind than the one declared by addon 'other' (models.rs:3); a \
         redeclaration keeps the kind"
    );
}

/// Review S1: same kind, native then runtime: refused naming both sources.
#[test]
fn runtime_declaration_of_the_same_kind_cannot_replace_a_native_object() {
    let native = ModelDecl::new("other", "x.t")
        .fields(const { &[FieldDecl::date("a"), FieldDecl::date("b")] })
        .table_objects(const { &[check_object("_guard")] })
        .at("models.rs", 3);
    let mut runtime = ModelDecl::extension("other", "x.t", &["x.t"])
        .table_objects(const { &[check_object("_guard")] })
        .at("models/guard.toml", 7);
    runtime.runtime = true;
    let err = build(&[&OTHER], vec![native, runtime]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "SQL object 'x_t_guard' of model 'x.t' is declared by Rust code in addon 'other' \
         (models.rs:3) and again at runtime in addon 'other' (models/guard.toml:7); a model file \
         or overlay cannot replace a native object, choose another name"
    );
}

/// Review S2: object names of runtime declarations are PostgreSQL names of at most 63 bytes.
#[test]
fn runtime_object_names_must_be_identifiers() {
    for name in ["_a b", "_a;b", "_a\nb", "_a\"b"] {
        let name: &'static str = Box::leak(name.to_owned().into_boxed_str());
        let mut decl = ModelDecl::new("other", "x.t")
            .table_objects(Box::leak(Box::new([check_object(name)])))
            .at("models/t.toml", 2);
        decl.runtime = true;
        let err = build(&[&OTHER], vec![decl]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "Names of SQL objects declared at runtime can only contain characters, digits and \
                 underscores (up to 63): '{}' on model 'x.t' (addon 'other', models/t.toml:2)",
                name.escape_debug()
            )
        );
    }
    let long: &'static str = Box::leak(format!("_{}", "n".repeat(63)).into_boxed_str());
    let mut decl = ModelDecl::new("other", "x.t")
        .table_objects(Box::leak(Box::new([check_object(long)])))
        .at("models/t.toml", 2);
    decl.runtime = true;
    let err = build(&[&OTHER], vec![decl]).unwrap_err();
    assert_eq!(
        err.message(),
        format!(
            "Names of SQL objects declared at runtime can only contain characters, digits and \
             underscores (up to 63): '{long}' on model 'x.t' (addon 'other', models/t.toml:2)"
        )
    );
}

/// Review S1 (delta): runtime object names may not be generated names.
#[test]
fn runtime_object_names_cannot_be_generated_names() {
    let names = [
        "_pkey",
        "_partner_id_fkey",
        "_title_not_null",
        "__title_index",
    ];
    for suffix in names {
        let name: &'static str = suffix;
        let mut decl = ModelDecl::new("other", "x.t")
            .fields(const { &[FieldDecl::char("title")] })
            .table_objects(Box::leak(Box::new([check_object(name)])))
            .at("models/t.toml", 4);
        decl.runtime = true;
        let err = build(&[&OTHER], vec![decl]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        let full = format!("x_t{suffix}");
        assert_eq!(
            err.message(),
            format!(
                "SQL object '{full}' of model 'x.t' (addon 'other', models/t.toml:4) has a name \
                 the ORM or PostgreSQL generates for a primary key, foreign key, not-null \
                 constraint or field index; choose another name"
            )
        );
    }
}

/// a many2one to a model that is not in the registry is a build error.
#[test]
fn unknown_many2one_target_is_an_error() {
    let err = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.link")
                .fields(const { &[FieldDecl::many2one("thing_id", "no.such.model")] }),
        ],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Field x.link.thing_id refers to unknown model 'no.such.model'"
    );
}

/// Field names become column names: same rule as table names.
#[test]
fn field_names_are_validated() {
    let err = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.bad").fields(const { &[FieldDecl::char("bad-name")] })],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(err.message().contains("'bad-name'"), "{err}");
    let err = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.nul").description("a\0b")],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("NUL"), "{err}");
}

#[test]
fn core_users_model_exists_for_audit_fields() {
    let registry = build(&[&OTHER], vec![]).unwrap();
    let users = registry.model("res.users").unwrap();
    assert!(users.field("name").unwrap().required);
    assert!(users.inherits("base") && users.inherits("res.users") && !users.inherits("x.y"));
}

/// Declarations of addon `base` from another crate (whose source path sorts before
/// `erp-registry`'s) come after the core `base` model, so they can inherit it.
#[test]
fn core_declarations_come_first_inside_their_addon() {
    static EARLY: ModelDecl = ModelDecl::new("base", "test.early.base")
        .description("Early")
        .fields(const { &[FieldDecl::char("name")] })
        .at("crates/aaa-early/src/models.rs", 1);
    static EARLY_EXT: ModelDecl = ModelDecl::extension("base", "res.users", &["res.users"])
        .fields(const { &[FieldDecl::char("early_note")] })
        .at("crates/aaa-early/src/models.rs", 2);
    let registry = RegistryBuilder::new([])
        .with_decls([&EARLY, &EARLY_EXT])
        .linked(false)
        .build()
        .unwrap();
    let early = registry.model("test.early.base").unwrap();
    assert!(early.mro.iter().any(|d| d.name == "base"));
    let users = registry.model("res.users").unwrap();
    assert!(users.field("early_note").is_some());
    assert!(users.field("login").is_some());
}

// ---------------------------------------------------------------------------------------------
// Signal triggers

fn trigger(name: &'static str, decl: SignalTriggerDecl) -> TableObjectDecl {
    TableObjectDecl {
        name,
        kind: TableObjectKind::SignalTrigger(decl),
    }
}

fn triggered_model(objects: Vec<TableObjectDecl>) -> ModelDecl {
    let objects: &'static [TableObjectDecl] = Box::leak(objects.into_boxed_slice());
    ModelDecl::new("other", "x.grp")
        .fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2many("implied_ids", "x.grp").relation(
                        "x_grp_implied_rel",
                        "gid",
                        "hid",
                    ),
                    FieldDecl::many2many("tag_ids", "x.grp").store(false),
                    FieldDecl::many2one("parent_id", "x.grp"),
                ]
            },
        )
        .table_objects(objects)
}

/// Declared triggers keep their table, signal and events; the relation one resolves to the
/// many2many's relation table.
#[test]
fn signal_triggers_resolve_their_tables() {
    let policy = SignalTriggerDecl::new(TriggerSignal::Policy);
    let registry = build(
        &[&OTHER],
        vec![triggered_model(vec![
            trigger("_policy_bump", policy),
            trigger("_policy_bump_implied", policy.on_relation("implied_ids")),
            trigger("_policy_bump_inserts", policy.without_updates()),
        ])],
    )
    .unwrap();
    let model = registry.model("x.grp").unwrap();
    let tables: Vec<(String, &str, &str)> = model
        .table_objects
        .iter()
        .map(|o| {
            let TableObjectKind::SignalTrigger(decl) = o.kind else {
                panic!("{} is not a signal trigger", o.full_name);
            };
            (
                o.full_name.clone(),
                model.signal_trigger_table(&decl).unwrap(),
                decl.events(),
            )
        })
        .collect();
    assert_eq!(
        tables,
        vec![
            (
                "x_grp_policy_bump".to_owned(),
                "x_grp",
                "INSERT OR UPDATE OR DELETE OR TRUNCATE"
            ),
            (
                "x_grp_policy_bump_implied".to_owned(),
                "x_grp_implied_rel",
                "INSERT OR UPDATE OR DELETE OR TRUNCATE"
            ),
            (
                "x_grp_policy_bump_inserts".to_owned(),
                "x_grp",
                "INSERT OR DELETE OR TRUNCATE"
            ),
        ]
    );
    assert_eq!(TriggerSignal::Policy.key(), "policy");
    assert_eq!(
        TriggerSignal::from_key("policy"),
        Some(TriggerSignal::Policy)
    );
    assert_eq!(TriggerSignal::from_key("registry"), None);
}

/// A relation trigger must name a stored many2many of the model.
#[test]
fn signal_trigger_relations_are_checked() {
    let policy = SignalTriggerDecl::new(TriggerSignal::Policy);
    for (field, why) in [
        ("missing_ids", "the model has no such field"),
        ("tag_ids", "the many2many is not stored (no relation table)"),
        ("parent_id", "the field is a many2one, not a many2many"),
        ("name", "the field is a char, not a many2many"),
    ] {
        let err = build(
            &[&OTHER],
            vec![triggered_model(vec![trigger(
                "_policy_bump_rel",
                policy.on_relation(field),
            )])],
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{field}");
        assert_eq!(
            err.message(),
            format!("signal trigger on the relation of x.grp.{field}: {why}"),
        );
    }
}

/// A trigger on a model without a table is refused.
#[test]
fn signal_triggers_need_a_table() {
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([trigger(
        "_policy_bump",
        SignalTriggerDecl::new(TriggerSignal::Policy),
    )]));
    let err = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.mixin")
                .kind(ModelKind::Abstract)
                .table_objects(objects),
        ],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "signal trigger 'x_mixin_policy_bump' on model 'x.mixin': the model has no table"
    );
}

// ---------------------------------------------------------------------------------------------
// Follow-delete triggers (review round 1, design)

fn follow(name: &'static str, decl: erp_registry::FollowDeleteDecl) -> TableObjectDecl {
    TableObjectDecl {
        name,
        kind: TableObjectKind::FollowDelete(decl),
    }
}

/// The key table of the follow-delete tests.
fn key_model() -> ModelDecl {
    ModelDecl::new("other", "x.key")
        .fields(const { &[FieldDecl::char("name"), FieldDecl::char("type")] })
}

/// Follow-delete triggers resolve their table like signal triggers, and every name is checked
/// when the registry is built.
#[test]
fn follow_delete_triggers_are_checked() {
    let keys = TableObjectKind::follow_delete("actions").deleting_keys("x_key");
    let registry = build(
        &[&OTHER],
        vec![
            key_model(),
            triggered_model(vec![
                follow("_keys_follow", keys),
                follow(
                    "_implied_follow",
                    TableObjectKind::follow_delete("actions").on_relation("implied_ids"),
                ),
            ]),
        ],
    )
    .unwrap();
    let model = registry.model("x.grp").unwrap();
    let tables: Vec<(String, &str)> = model
        .table_objects
        .iter()
        .map(|o| {
            let TableObjectKind::FollowDelete(decl) = o.kind else {
                panic!("{} is not a follow-delete trigger", o.full_name);
            };
            (
                o.full_name.clone(),
                model.follow_delete_table(&decl).unwrap(),
            )
        })
        .collect();
    assert_eq!(
        tables,
        vec![
            ("x_grp_keys_follow".to_owned(), "x_grp"),
            ("x_grp_implied_follow".to_owned(), "x_grp_implied_rel"),
        ]
    );

    let long = "c".repeat(erp_registry::MAX_FOLLOW_CACHE_LEN + 1);
    let long: &'static str = Box::leak(long.into_boxed_str());
    for (decl, why) in [
        (
            TableObjectKind::follow_delete("Actions"),
            "the cache name 'Actions' is not 1 to 40 characters of [a-z0-9_]".to_owned(),
        ),
        (
            TableObjectKind::follow_delete("a.b"),
            "the cache name 'a.b' is not 1 to 40 characters of [a-z0-9_]".to_owned(),
        ),
        (
            TableObjectKind::follow_delete(long),
            format!("the cache name '{long}' is not 1 to 40 characters of [a-z0-9_]"),
        ),
        (
            TableObjectKind::follow_delete("actions")
                .on_relation("implied_ids")
                .deleting_keys("x_key"),
            "a trigger on a relation table deletes no rows of 'x_key' (relation rows carry no \
             ids of its key space)"
                .to_owned(),
        ),
        (
            TableObjectKind::follow_delete("actions").deleting_keys("x_grp"),
            "the key table 'x_grp' is the model's own table".to_owned(),
        ),
        (
            TableObjectKind::follow_delete("actions").deleting_keys("x_nothing"),
            "the key table 'x_nothing' is not the table of a model in the registry".to_owned(),
        ),
    ] {
        let err = build(
            &[&OTHER],
            vec![key_model(), triggered_model(vec![follow("_follow", decl)])],
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{why}");
        assert_eq!(
            err.message(),
            format!("follow-delete trigger 'x_grp_follow' on model 'x.grp': {why}")
        );
    }
    // a key table whose rows do not name their model
    let err = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.untyped").fields(const { &[FieldDecl::char("name")] }),
            triggered_model(vec![follow(
                "_follow",
                TableObjectKind::follow_delete("actions").deleting_keys("x_untyped"),
            )]),
        ],
    )
    .unwrap_err();
    assert_eq!(
        err.message(),
        "follow-delete trigger 'x_grp_follow' on model 'x.grp': the key table 'x_untyped' (model \
         'x.untyped') has no stored char field 'type' naming the model of each key row"
    );
    let err = build(
        &[&OTHER],
        vec![triggered_model(vec![follow(
            "_follow",
            TableObjectKind::follow_delete("actions").on_relation("tag_ids"),
        )])],
    )
    .unwrap_err();
    assert_eq!(
        err.message(),
        "follow-delete trigger on the relation of x.grp.tag_ids: the many2many is not stored (no \
         relation table)"
    );
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([follow(
        "_follow",
        TableObjectKind::follow_delete("actions"),
    )]));
    let err = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.mixin")
                .kind(ModelKind::Abstract)
                .table_objects(objects),
        ],
    )
    .unwrap_err();
    assert_eq!(
        err.message(),
        "follow-delete trigger 'x_mixin_follow' on model 'x.mixin': the model has no table"
    );
    assert!(erp_registry::ModelDecl::valid_id_sequence(
        "ir_actions_id_seq"
    ));
    assert!(!erp_registry::ModelDecl::valid_id_sequence("1seq"));
}

// ---------------------------------------------------------------------------------------------
// Restricted field parameters (R12)

/// A model that did not allow `user_writeable` refuses a field that sets it, whatever the
/// model is called.
#[test]
fn restricted_field_parameter_needs_the_model_to_allow_it() {
    let err = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.principal")
                .fields(const { &[FieldDecl::char("tz").user_writeable(true)] }),
        ],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Field x.principal.tz sets user_writeable, which the model does not allow \
         (declare field_parameters = [\"user_writeable\"] on it)"
    );
}

/// The allow-list of any declaration of the model counts, so an extension may add such a field.
#[test]
fn restricted_field_parameter_allowed_by_the_model_owner() {
    let registry = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.principal")
                .field_parameters(&["user_writeable"])
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::extension("other", "x.principal", &["x.principal"])
                .fields(const { &[FieldDecl::char("tz").user_writeable(true)] }),
        ],
    )
    .unwrap();
    assert!(
        registry
            .model("x.principal")
            .and_then(|m| m.field("tz"))
            .is_some_and(|f| f.user_writeable)
    );
}

/// A related field takes the groups of its target; a path that does not resolve and a cycle of
/// related fields are registry build errors naming `model.field` (not "no groups", which would
/// leave a restricted target readable through the field).
#[test]
fn related_groups_follow_the_path_and_refuse_broken_paths() {
    let registry = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.rel").fields(
            const {
                &[
                    FieldDecl::char("secret").groups("base.group_system"),
                    FieldDecl::char("a").related("secret"),
                    FieldDecl::char("b").related("a"),
                ]
            },
        )],
    )
    .unwrap();
    let groups = |name: &str| registry.model("x.rel").unwrap().field(name).unwrap().groups;
    assert_eq!(groups("a"), Some("base.group_system"));
    assert_eq!(groups("b"), Some("base.group_system"));

    let err = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.rel")
                .fields(const { &[FieldDecl::char("a").related("missing")] }),
        ],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "related field x.rel.a (related='missing'): 'missing' is not a field of model 'x.rel'"
    );

    let err = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.rel").fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::char("a").related("name.name"),
                ]
            },
        )],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "related field x.rel.a (related='name.name'): the path goes through x.rel.name, which is \
         not a relation field"
    );

    let err = build(
        &[&OTHER],
        vec![ModelDecl::new("other", "x.rel").fields(
            const {
                &[
                    FieldDecl::char("a").related("b"),
                    FieldDecl::char("b").related("a"),
                ]
            },
        )],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "related fields form a cycle: x.rel.a -> x.rel.b -> x.rel.a"
    );

    let err = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.rel").fields(const { &[FieldDecl::char("a").related("a")] }),
        ],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "related fields form a cycle: x.rel.a -> x.rel.a"
    );
}

#[test]
fn spec_field_081_override_company_dependent_field() {
    static EXT: [&str; 1] = ["test.inherit.prop"];
    let base = ModelDecl::new("mother_base", "test.inherit.prop").fields(
        const {
            &[
                FieldDecl::integer("property_foo").company_dependent(true),
                FieldDecl::integer("property_bar").company_dependent(true),
            ]
        },
    );
    let ext = ModelDecl::extension("mother_inherit_1", "test.inherit.prop", &EXT).fields(
        const {
            &[
                FieldDecl::integer("property_foo").company_dependent(false),
                FieldDecl::integer("property_bar")
                    .compute("_compute_bar")
                    .company_dependent(false),
            ]
        },
    );
    let reg = build(&[&MOTHER_BASE, &MOTHER_1], vec![base, ext]).unwrap();
    let m = reg.model("test.inherit.prop").unwrap();
    let foo = m.field("property_foo").unwrap();
    let bar = m.field("property_bar").unwrap();
    assert!(!foo.company_dependent);
    assert!(!bar.company_dependent);
    assert!(foo.has_column());
    assert!(!bar.has_column());
    assert_eq!(foo.column(), Some(ColumnType::Int4));
    assert_eq!(bar.column(), None);
    assert_ne!(foo.column(), Some(ColumnType::Jsonb));
    assert_ne!(bar.column(), Some(ColumnType::Jsonb));
}

#[test]
fn spec_field_007_related_field_defaults() {
    let base = ModelDecl::new("other", "x.rel.target")
        .fields(const { &[FieldDecl::char("text"), FieldDecl::integer("num")] });
    let rel_model = ModelDecl::new("other", "x.rel.host").fields(
        const {
            &[
                FieldDecl::many2one("target_id", "x.rel.target"),
                FieldDecl::char("text1").related("target_id.text"),
                FieldDecl::char("text2")
                    .related("target_id.text")
                    .readonly(false),
            ]
        },
    );
    let reg = build(&[&OTHER], vec![base, rel_model]).unwrap();
    let host = reg.model("x.rel.host").unwrap();
    let text1 = host.field("text1").unwrap();
    assert!(!text1.store);
    assert!(text1.readonly);
    assert!(!text1.copy);
    assert!(text1.compute_sudo());

    let text2 = host.field("text2").unwrap();
    assert!(!text2.readonly);
}

#[test]
fn spec_field_080_selection_redefined_as_method() {
    static EXT: [&str; 1] = ["test.sel.base"];
    let base = ModelDecl::new("mother_base", "test.sel.base").fields(
        const {
            &[FieldDecl::selection(
                "other",
                &[("foo", "Foo"), ("bar", "Bar")],
            )]
        },
    );
    let ext = ModelDecl::extension("mother_inherit_1", "test.sel.base", &EXT).fields(
        const {
            &[FieldDecl::new(
                "other",
                KindDecl::Selection {
                    values: None,
                    method: Some("_other_values"),
                    add: &[],
                    ondelete: &[],
                },
            )]
        },
    );
    let reg = build(&[&MOTHER_BASE, &MOTHER_1], vec![base, ext]).unwrap();
    let m = reg.model("test.sel.base").unwrap();
    let other = m.field("other").unwrap();
    assert_eq!(
        other.ty,
        FieldType::Selection {
            values: None,
            method: Some("_other_values".into()),
        }
    );
}

#[test]
fn spec_model_024_locally_redefined_field_not_inherited() {
    let mother = ModelDecl::new("mother_base", "test.inherit.mother").fields(
        const {
            &[FieldDecl::char("name")
                .required(true)
                .default(StaticValue::Str("Bar"))]
        },
    );
    let daughter = ModelDecl::new("other", "test.inherit.daughter")
        .inherits(&[("test.inherit.mother", "mother_id")])
        .fields(
            const {
                &[
                    FieldDecl::many2one("mother_id", "test.inherit.mother")
                        .required(true)
                        .ondelete(OnDelete::Cascade),
                    FieldDecl::char("name").default(StaticValue::Str("Baz")),
                ]
            },
        );
    let registry = build(&[&MOTHER_BASE, &OTHER], vec![mother, daughter]).unwrap();
    let mother_name = registry
        .model("test.inherit.mother")
        .unwrap()
        .field("name")
        .unwrap();
    assert!(mother_name.required);
    assert!(matches!(
        mother_name.default,
        Some(FieldDefault::Value(StaticValue::Str("Bar")))
    ));
    assert!(mother_name.inherited.is_none());

    let daughter_name = registry
        .model("test.inherit.daughter")
        .unwrap()
        .field("name")
        .unwrap();
    assert!(!daughter_name.required);
    assert!(matches!(
        daughter_name.default,
        Some(FieldDefault::Value(StaticValue::Str("Baz")))
    ));
    assert!(daughter_name.store);
    assert!(daughter_name.inherited.is_none());
}

#[test]
fn spec_model_028_setup_result_independent_of_model_iteration_order() {
    let make_decls = || {
        let mixin = ModelDecl::new("other", "x.display.mixin")
            .kind(ModelKind::Abstract)
            .fields(const { &[FieldDecl::char("display_name").compute("_compute_display_name")] });
        let parent = ModelDecl::new("other", "x.display.parent")
            .fields(const { &[FieldDecl::char("display_name").store(true)] });
        let child =
            ModelDecl::new("other", "x.display").inherit(&["x.display.mixin", "x.display.parent"]);
        (mixin, parent, child)
    };

    // First declaration order: mixin, parent, child
    let (mixin, parent, child) = make_decls();
    let reg1 = build(&[&OTHER], vec![mixin, parent, child]).unwrap();
    assert!(
        reg1.model("x.display")
            .unwrap()
            .field("display_name")
            .unwrap()
            .store
    );

    // Second declaration order: parent, mixin, child
    let (mixin, parent, child) = make_decls();
    let reg2 = build(&[&OTHER], vec![parent, mixin, child]).unwrap();
    assert!(
        reg2.model("x.display")
            .unwrap()
            .field("display_name")
            .unwrap()
            .store
    );
}

fn leak<T>(items: Vec<T>) -> &'static [T] {
    Box::leak(items.into_boxed_slice())
}

#[test]
fn spec_field_061_translate_column_resolves_to_jsonb() {
    let decls = vec![ModelDecl::new("other", "x.translated.test").fields(
        const {
            &[
                FieldDecl::char("name").translate(true),
                FieldDecl::text("description").translate(true),
                FieldDecl::html("body").translate(true),
                FieldDecl::char("untranslated"),
            ]
        },
    )];
    let reg = build(&[&OTHER], decls).unwrap();
    let model = reg.model("x.translated.test").unwrap();

    let name = model.field("name").unwrap();
    assert!(name.translate);
    assert_eq!(name.column(), Some(ColumnType::Jsonb));

    let desc = model.field("description").unwrap();
    assert!(desc.translate);
    assert_eq!(desc.column(), Some(ColumnType::Jsonb));

    let body = model.field("body").unwrap();
    assert!(body.translate);
    assert_eq!(body.column(), Some(ColumnType::Jsonb));

    let untranslated = model.field("untranslated").unwrap();
    assert!(!untranslated.translate);
    assert_eq!(untranslated.column(), Some(ColumnType::Varchar(None)));
}

#[test]
fn spec_field_061_translate_extension_override() {
    let decls = vec![
        ModelDecl::new("mother_base", "test.inherit.translate")
            .fields(const { &[FieldDecl::char("note")] }),
        ModelDecl::extension(
            "mother_inherit_1",
            "test.inherit.translate",
            &["test.inherit.translate"],
        )
        .fields(const { &[FieldDecl::char("note").translate(true)] }),
    ];
    let reg = build(&[&MOTHER_BASE, &MOTHER_1], decls).unwrap();
    let model = reg.model("test.inherit.translate").unwrap();
    let note = model.field("note").unwrap();
    assert!(note.translate);
    assert_eq!(note.column(), Some(ColumnType::Jsonb));
}

#[test]
fn spec_field_061_translate_extension_disables() {
    let decls = vec![
        ModelDecl::new("mother_base", "test.inherit.translate")
            .fields(const { &[FieldDecl::char("note").translate(true)] }),
        ModelDecl::extension(
            "mother_inherit_1",
            "test.inherit.translate",
            &["test.inherit.translate"],
        )
        .fields(const { &[FieldDecl::char("note").translate(false)] }),
    ];
    let reg = build(&[&MOTHER_BASE, &MOTHER_1], decls).unwrap();
    let note = reg
        .model("test.inherit.translate")
        .unwrap()
        .field("note")
        .unwrap();
    assert!(!note.translate);
    assert_eq!(note.column(), Some(ColumnType::Varchar(None)));
}

#[test]
fn spec_field_061_non_string_translate_refused() {
    let cases = [
        ("integer", FieldDecl::integer("f_int").translate(true)),
        ("boolean", FieldDecl::boolean("f_bool").translate(true)),
        ("date", FieldDecl::date("f_date").translate(true)),
        ("float", FieldDecl::float("f_float").translate(true)),
        (
            "many2one",
            FieldDecl::many2one("f_m2o", "test.inherit.mother").translate(true),
        ),
    ];
    for (ty_name, fd) in cases {
        let decls = vec![ModelDecl::new("other", "x.invalid.translate").fields(leak(vec![fd]))];
        let err = build(&[&OTHER], decls).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "field 'x.invalid.translate.{}': only Char, Text, and Html fields can be translated (this field is {ty_name})",
                fd.name
            )
        );
    }
}

#[test]
fn spec_field_061_both_translated_and_company_dependent_refused() {
    let decls = vec![ModelDecl::new("other", "x.both.test").fields(
        const {
            &[FieldDecl::char("title")
                .translate(true)
                .company_dependent(true)]
        },
    )];
    let err = build(&[&OTHER], decls).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "field 'x.both.test.title': a field cannot be both translated and company-dependent"
    );
}

#[test]
fn spec_field_025_rejects_wrong_currency_type_and_model() {
    for field in [
        FieldDecl::char("currency_id"),
        FieldDecl::many2one("currency_id", "x.other"),
    ] {
        let fields = Box::leak(
            vec![
                field,
                FieldDecl::new(
                    "amount",
                    KindDecl::Monetary {
                        currency_field: Some("currency_id"),
                    },
                ),
            ]
            .into_boxed_slice(),
        );
        let error = build(
            &[&OTHER],
            vec![
                ModelDecl::new("other", "res.currency"),
                ModelDecl::new("other", "x.other"),
                ModelDecl::new("other", "x.money").fields(fields),
            ],
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "Field x.money.amount currency_field currency_id must be a many2one to res.currency"
        );
    }
}

#[test]
fn spec_field_025_accepts_related_currency() {
    let registry = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "res.currency"),
            ModelDecl::new("other", "x.owner")
                .fields(const { &[FieldDecl::many2one("currency_id", "res.currency")] }),
            ModelDecl::new("other", "x.money").fields(
                const {
                    &[
                        FieldDecl::many2one("owner_id", "x.owner"),
                        FieldDecl::many2one("currency_id", "res.currency")
                            .related("owner_id.currency_id"),
                        FieldDecl::new(
                            "amount",
                            KindDecl::Monetary {
                                currency_field: Some("currency_id"),
                            },
                        ),
                    ]
                },
            ),
        ],
    )
    .unwrap();
    assert!(
        matches!(&registry.model("x.money").unwrap().field("currency_id").unwrap().ty,
        FieldType::Many2one { comodel, .. } if comodel == "res.currency")
    );
}

#[test]
fn spec_field_025_validates_inherited_currency_model() {
    for valid in [true, false] {
        let fields = Box::leak(
            vec![FieldDecl::many2one(
                "currency_id",
                if valid { "res.currency" } else { "x.other" },
            )]
            .into_boxed_slice(),
        );
        let result = build(
            &[&OTHER],
            vec![
                ModelDecl::new("other", "res.currency"),
                ModelDecl::new("other", "x.other"),
                ModelDecl::new("other", "x.owner").fields(fields),
                ModelDecl::new("other", "x.money")
                    .inherits(&[("x.owner", "owner_id")])
                    .fields(
                        const {
                            &[
                                FieldDecl::many2one("owner_id", "x.owner")
                                    .delegate(true)
                                    .required(true)
                                    .ondelete(OnDelete::Cascade),
                                FieldDecl::new(
                                    "amount",
                                    KindDecl::Monetary {
                                        currency_field: Some("currency_id"),
                                    },
                                ),
                            ]
                        },
                    ),
            ],
        );
        if valid {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(
                error.message(),
                "Field x.money.amount currency_field currency_id must be a many2one to res.currency"
            );
        }
    }
}

#[test]
fn spec_field_025_rejects_misdeclared_related_currency_alias() {
    let error = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "res.currency"),
            ModelDecl::new("other", "x.other"),
            ModelDecl::new("other", "x.owner")
                .fields(const { &[FieldDecl::many2one("other_id", "x.other")] }),
            ModelDecl::new("other", "x.money").fields(
                const {
                    &[
                        FieldDecl::many2one("owner_id", "x.owner"),
                        FieldDecl::many2one("currency_id", "res.currency")
                            .related("owner_id.other_id"),
                        FieldDecl::new(
                            "amount",
                            KindDecl::Monetary {
                                currency_field: Some("currency_id"),
                            },
                        ),
                    ]
                },
            ),
        ],
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Field x.money.amount currency_field currency_id must be a many2one to res.currency"
    );
}

#[test]
fn spec_field_025_checks_currency_alias_chains() {
    for valid in [false, true] {
        for shape in [0, 1, 2] {
            let actual = if valid { "res.currency" } else { "x.other" };
            let mut decls = vec![
                ModelDecl::new("other", "res.currency"),
                ModelDecl::new("other", "x.other"),
            ];
            let amount = FieldDecl::new(
                "amount",
                KindDecl::Monetary {
                    currency_field: Some("currency_id"),
                },
            );
            if shape == 2 {
                decls.push(
                    ModelDecl::new("other", "x.owner").fields(Box::leak(
                        vec![
                            FieldDecl::many2one("actual_id", actual),
                            FieldDecl::many2one("currency_id", "res.currency").related("actual_id"),
                        ]
                        .into_boxed_slice(),
                    )),
                );
                decls.push(
                    ModelDecl::new("other", "x.money")
                        .inherits(&[("x.owner", "owner_id")])
                        .fields(Box::leak(
                            vec![
                                FieldDecl::many2one("owner_id", "x.owner")
                                    .delegate(true)
                                    .required(true)
                                    .ondelete(OnDelete::Cascade),
                                amount,
                            ]
                            .into_boxed_slice(),
                        )),
                );
            } else {
                let mut fields = vec![FieldDecl::many2one("actual_id", actual), amount];
                if shape == 1 {
                    fields
                        .push(FieldDecl::many2one("alias_id", "res.currency").related("actual_id"));
                }
                fields.push(
                    FieldDecl::many2one("currency_id", "res.currency").related(if shape == 0 {
                        "actual_id"
                    } else {
                        "alias_id"
                    }),
                );
                decls.push(
                    ModelDecl::new("other", "x.money").fields(Box::leak(fields.into_boxed_slice())),
                );
            }
            let result = build(&[&OTHER], decls);
            if valid {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Programming);
                assert_eq!(
                    error.message(),
                    "Field x.money.amount currency_field currency_id must be a many2one to res.currency"
                );
            }
        }
    }
}

static SECRET_BASE: AddonDescriptor = AddonDescriptor {
    name: "secret_base",
    depends: &["base"],
    exact_only: false,
};
static SECRET_EXT: AddonDescriptor = AddonDescriptor {
    name: "secret_ext",
    depends: &["secret_base"],
    exact_only: false,
};

/// An extension that re-declares a `secret` field with `secret = false` is refused, naming the
/// field and both addons; one that omits the attribute keeps it, and one can turn it on
/// (delta D2).
#[test]
fn an_extension_cannot_switch_secret_off() {
    let base_decl = || {
        ModelDecl::new("secret_base", "x.sec")
            .fields(const { &[FieldDecl::char("pw").secret(true), FieldDecl::char("late")] })
    };
    let err = build(
        &[&SECRET_BASE, &SECRET_EXT],
        vec![
            base_decl(),
            ModelDecl::extension("secret_ext", "x.sec", &["x.sec"])
                .fields(const { &[FieldDecl::char("pw").secret(false)] }),
        ],
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    let message = err.to_string();
    for part in [
        "x.sec.pw",
        "secret_base",
        "secret_ext",
        "cannot switch `secret` off",
    ] {
        assert!(message.contains(part), "{part} in {message}");
    }

    let registry = build(
        &[&SECRET_BASE, &SECRET_EXT],
        vec![
            base_decl(),
            ModelDecl::extension("secret_ext", "x.sec", &["x.sec"]).fields(
                const {
                    &[
                        FieldDecl::char("pw").string("Renamed"),
                        FieldDecl::char("late").secret(true),
                    ]
                },
            ),
        ],
    )
    .unwrap();
    let secret = |name: &str| registry.model("x.sec").unwrap().field(name).unwrap().secret;
    assert!(secret("pw"), "an extension without the attribute keeps it");
    assert!(secret("late"), "an extension can turn it on");
}

/// A related field takes `secret` from its target, through a chain and through a relation hop.
#[test]
fn related_fields_inherit_secret() {
    let registry = build(
        &[&OTHER],
        vec![
            ModelDecl::new("other", "x.owner").fields(
                const {
                    &[
                        FieldDecl::char("pw").secret(true),
                        FieldDecl::char("plain"),
                        FieldDecl::char("a").related("pw"),
                        FieldDecl::char("b").related("a"),
                        FieldDecl::char("c").related("plain"),
                    ]
                },
            ),
            ModelDecl::new("other", "x.holder").fields(
                const {
                    &[
                        FieldDecl::many2one("owner_id", "x.owner"),
                        FieldDecl::char("hop").related("owner_id.pw"),
                        FieldDecl::char("hop_plain").related("owner_id.plain"),
                    ]
                },
            ),
        ],
    )
    .unwrap();
    let secret =
        |model: &str, name: &str| registry.model(model).unwrap().field(name).unwrap().secret;
    assert!(secret("x.owner", "a") && secret("x.owner", "b"));
    assert!(secret("x.holder", "hop"));
    assert!(!secret("x.owner", "c") && !secret("x.holder", "hop_plain"));
}
