//! Tests for selection ondelete policies at uninstall.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_data::XmlId;
use erp_data::load::purge_module_data;
use erp_data::model_data::{self, UNLINK_HOOK, XidBinding};
use erp_fields::{Cell, Error, Value};
use erp_orm::{SelectionCallback, WriteHook};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, SelectionOnDeleteDecl, StaticValue};
use erp_testkit::{Harness, HarnessError};

static MOD_BASE: AddonDescriptor = AddonDescriptor {
    name: "mod_base",
    depends: &["base"],
    exact_only: false,
};

static MOD_EXT: AddonDescriptor = AddonDescriptor {
    name: "mod_ext",
    depends: &["mod_base"],
    exact_only: false,
};

async fn alive(tx: &mut erp_orm::Transaction, model: &str, ids: &[i64]) -> Vec<i64> {
    let records = tx
        .env()
        .records(model)
        .unwrap()
        .browse(ids.to_vec())
        .unwrap();
    records.exists(tx).await.unwrap().ids()
}

async fn bind(tx: &mut erp_orm::Transaction, model: &str, pairs: &[(&str, i64)]) {
    let env = tx.env();
    let bindings: Vec<XidBinding> = pairs
        .iter()
        .map(|(x, id)| XidBinding::new(XmlId::parse(x).unwrap(), model, *id, false))
        .collect();
    model_data::update_xmlids(tx, &env, &bindings, false)
        .await
        .unwrap();
}

async fn has_xid(tx: &mut erp_orm::Transaction, text: &str) -> bool {
    let env = tx.env();
    let x = XmlId::parse(text).unwrap();
    model_data::lookup(tx, &env, std::slice::from_ref(&x))
        .await
        .unwrap()
        .contains_key(&x)
}

#[tokio::test]
async fn spec_field_035_test_ondelete_default() {
    const MODEL: &str = "test_ondelete.required_default";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Required with default")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("my_selection", &[("foo", "Foo"), ("bar", "Bar")])
                            .required(true)
                            .default(StaticValue::Str("foo")),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "my_selection",
                    &[("baz", Some("Baz"))],
                    &[("baz", SelectionOnDeleteDecl::SetDefault)],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records(MODEL).unwrap();
    let create = |val: &str| {
        let values: erp_sdk::ValueMap = [("my_selection", Value::str(val))]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        rows.vals_from_values(values.iter()).unwrap()
    };
    let rec1 = rows
        .create(&mut tx, vec![create("foo")])
        .await
        .unwrap()
        .ids()[0];
    let rec2 = rows
        .create(&mut tx, vec![create("bar")])
        .await
        .unwrap()
        .ids()[0];
    let rec3 = rows
        .create(&mut tx, vec![create("baz")])
        .await
        .unwrap()
        .ids()[0];

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());

    let recs = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![rec1, rec2, rec3])
        .unwrap();
    let vals = recs.read(&mut tx, &["my_selection"]).await.unwrap();
    assert_eq!(vals[0]["my_selection"], Value::str("foo"));
    assert_eq!(vals[1]["my_selection"], Value::str("bar"));
    assert_eq!(vals[2]["my_selection"], Value::str("foo")); // reset to default

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_035_test_ondelete_base_null_explicit() {
    const MODEL: &str = "test_ondelete.optional_explicit_null";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Optional explicit null")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("my_selection", &[("foo", "Foo"), ("bar", "Bar")]),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "my_selection",
                    &[("quux", Some("Quux"))],
                    &[("quux", SelectionOnDeleteDecl::SetNull)],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records(MODEL).unwrap();
    let create = |val: &str| {
        let values: erp_sdk::ValueMap = [("my_selection", Value::str(val))]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        rows.vals_from_values(values.iter()).unwrap()
    };
    let rec1 = rows
        .create(&mut tx, vec![create("foo")])
        .await
        .unwrap()
        .ids()[0];
    let rec2 = rows
        .create(&mut tx, vec![create("bar")])
        .await
        .unwrap()
        .ids()[0];
    let rec3 = rows
        .create(&mut tx, vec![create("quux")])
        .await
        .unwrap()
        .ids()[0];

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());

    let recs = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![rec1, rec2, rec3])
        .unwrap();
    let vals = recs.read(&mut tx, &["my_selection"]).await.unwrap();
    assert_eq!(vals[0]["my_selection"], Value::str("foo"));
    assert_eq!(vals[1]["my_selection"], Value::str("bar"));
    assert_eq!(vals[2]["my_selection"], Value::False);

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_035_test_ondelete_base_null_implicit() {
    const MODEL: &str = "test_ondelete.optional_implicit_null";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Optional implicit null")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("my_selection", &[("foo", "Foo"), ("bar", "Bar")]),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add(
                    "my_selection",
                    &[("ham", Some("Ham"))],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records(MODEL).unwrap();
    let create = |val: &str| {
        let values: erp_sdk::ValueMap = [("my_selection", Value::str(val))]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        rows.vals_from_values(values.iter()).unwrap()
    };
    let rec1 = rows
        .create(&mut tx, vec![create("foo")])
        .await
        .unwrap()
        .ids()[0];
    let rec2 = rows
        .create(&mut tx, vec![create("bar")])
        .await
        .unwrap()
        .ids()[0];
    let rec3 = rows
        .create(&mut tx, vec![create("ham")])
        .await
        .unwrap()
        .ids()[0];

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());

    let recs = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![rec1, rec2, rec3])
        .unwrap();
    let vals = recs.read(&mut tx, &["my_selection"]).await.unwrap();
    assert_eq!(vals[0]["my_selection"], Value::str("foo"));
    assert_eq!(vals[1]["my_selection"], Value::str("bar"));
    assert_eq!(vals[2]["my_selection"], Value::False);

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_035_test_ondelete_cascade() {
    const MODEL: &str = "test_ondelete.cascade_model";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Cascade model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("my_selection", &[("foo", "Foo"), ("bar", "Bar")])
                            .required(true),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "my_selection",
                    &[("eggs", Some("Eggs"))],
                    &[("eggs", SelectionOnDeleteDecl::Cascade)],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records(MODEL).unwrap();
    let create = |val: &str| {
        let values: erp_sdk::ValueMap = [("my_selection", Value::str(val))]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        rows.vals_from_values(values.iter()).unwrap()
    };
    let rec1 = rows
        .create(&mut tx, vec![create("foo")])
        .await
        .unwrap()
        .ids()[0];
    let rec2 = rows
        .create(&mut tx, vec![create("bar")])
        .await
        .unwrap()
        .ids()[0];
    let rec3 = rows
        .create(&mut tx, vec![create("eggs")])
        .await
        .unwrap()
        .ids()[0];

    bind(&mut tx, MODEL, &[("mod_ext.rec3_xmlid", rec3)]).await;
    assert!(has_xid(&mut tx, "mod_ext.rec3_xmlid").await);

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert_eq!(report.deleted, [(MODEL.to_owned(), 1)]);

    assert_eq!(alive(&mut tx, MODEL, &[rec1, rec2]).await, vec![rec1, rec2]);
    assert!(alive(&mut tx, MODEL, &[rec3]).await.is_empty());
    assert!(!has_xid(&mut tx, "mod_ext.rec3_xmlid").await);

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_035_test_ondelete_literal() {
    const MODEL: &str = "test_ondelete.literal_model";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Literal policy model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("my_selection", &[("foo", "Foo"), ("bar", "Bar")])
                            .required(true),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "my_selection",
                    &[("bacon", Some("Bacon"))],
                    &[("bacon", SelectionOnDeleteDecl::SetValue("bar"))],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records(MODEL).unwrap();
    let create = |val: &str| {
        let values: erp_sdk::ValueMap = [("my_selection", Value::str(val))]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        rows.vals_from_values(values.iter()).unwrap()
    };
    let rec1 = rows
        .create(&mut tx, vec![create("foo")])
        .await
        .unwrap()
        .ids()[0];
    let rec2 = rows
        .create(&mut tx, vec![create("bar")])
        .await
        .unwrap()
        .ids()[0];
    let rec3 = rows
        .create(&mut tx, vec![create("bacon")])
        .await
        .unwrap()
        .ids()[0];

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());

    let recs = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![rec1, rec2, rec3])
        .unwrap();
    let vals = recs.read(&mut tx, &["my_selection"]).await.unwrap();
    assert_eq!(vals[0]["my_selection"], Value::str("foo"));
    assert_eq!(vals[1]["my_selection"], Value::str("bar"));
    assert_eq!(vals[2]["my_selection"], Value::str("bar")); // bacon -> bar

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_035_test_ondelete_multiple_explicit() {
    const MODEL: &str = "test_ondelete.multiple_explicit";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Multiple explicit policies")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("my_selection", &[("foo", "Foo"), ("bar", "Bar")])
                            .required(true)
                            .default(StaticValue::Str("foo")),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "my_selection",
                    &[("eevee", Some("Eevee")), ("pikachu", Some("Pikachu"))],
                    &[
                        ("eevee", SelectionOnDeleteDecl::SetDefault),
                        ("pikachu", SelectionOnDeleteDecl::SetValue("bar")),
                    ],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records(MODEL).unwrap();
    let create = |val: &str| {
        let values: erp_sdk::ValueMap = [("my_selection", Value::str(val))]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        rows.vals_from_values(values.iter()).unwrap()
    };
    let rec1 = rows
        .create(&mut tx, vec![create("foo")])
        .await
        .unwrap()
        .ids()[0];
    let rec2 = rows
        .create(&mut tx, vec![create("eevee")])
        .await
        .unwrap()
        .ids()[0];
    let rec3 = rows
        .create(&mut tx, vec![create("pikachu")])
        .await
        .unwrap()
        .ids()[0];

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());

    let recs = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![rec1, rec2, rec3])
        .unwrap();
    let vals = recs.read(&mut tx, &["my_selection"]).await.unwrap();
    assert_eq!(vals[0]["my_selection"], Value::str("foo"));
    assert_eq!(vals[1]["my_selection"], Value::str("foo")); // eevee -> default ("foo")
    assert_eq!(vals[2]["my_selection"], Value::str("bar")); // pikachu -> "bar"

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_035_test_ondelete_callback() {
    const MODEL: &str = "test_ondelete.callback_model";
    let callback = SelectionCallback::new("test_archive_cb", |tx, model, ids| {
        Box::pin(async move {
            let env = tx.env();
            let rec_ids: Vec<i64> = ids.iter().map(|id| id.get()).collect();
            let records = env.records(model)?.browse(rec_ids)?;
            let mut map = std::collections::BTreeMap::new();
            map.insert("my_selection".to_owned(), Value::str("foo"));
            map.insert("active".to_owned(), Value::Bool(false));
            records.write_values(tx, &map).await?;
            Ok(())
        })
    });

    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Callback policy model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::boolean("active").default(StaticValue::Bool(true)),
                        FieldDecl::selection("my_selection", &[("foo", "Foo"), ("bar", "Bar")])
                            .required(true),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "my_selection",
                    &[("knickers", Some("Knickers"))],
                    &[(
                        "knickers",
                        SelectionOnDeleteDecl::Callback("test_archive_cb"),
                    )],
                )]
            },
        ),
    ])
    .selection_callback(callback)
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records(MODEL).unwrap();
    let create = |val: &str| {
        let values: erp_sdk::ValueMap = [("my_selection", Value::str(val))]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        rows.vals_from_values(values.iter()).unwrap()
    };
    let rec = rows
        .create(&mut tx, vec![create("knickers")])
        .await
        .unwrap()
        .ids()[0];

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());

    let recs = tx.env().records(MODEL).unwrap().browse(vec![rec]).unwrap();
    let vals = recs
        .read(&mut tx, &["my_selection", "active"])
        .await
        .unwrap();
    assert_eq!(vals[0]["my_selection"], Value::str("foo"));
    assert_eq!(vals[0]["active"], Value::Bool(false));

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_035_test_non_stored_selection() {
    const MODEL: &str = "test_ondelete.non_stored";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Non stored selection")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("my_selection", &[("foo", "Foo")]).store(false),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add(
                    "my_selection",
                    &[("bar", Some("Bar"))],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records(MODEL).unwrap();
    let values: erp_sdk::ValueMap = [("name", Value::str("test"))]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    let vals = rows.vals_from_values(values.iter()).unwrap();
    let rec = rows.create(&mut tx, vec![vals]).await.unwrap().ids()[0];

    // Purging mod_ext does not try to update non-stored columns
    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert_eq!(alive(&mut tx, MODEL, &[rec]).await, vec![rec]);
    assert!(report.unavailable_models.is_empty());

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_035_test_required_base_selection_field() {
    const MODEL: &str = "test_ondelete.required_base";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Required base selection")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("my_selection", &[("foo", "Foo"), ("bar", "Bar")])
                            .required(true),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL])
            .fields(const { &[FieldDecl::char("extra_field")] }),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records(MODEL).unwrap();
    let values: erp_sdk::ValueMap = [("my_selection", Value::str("foo"))]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    let vals = rows.vals_from_values(values.iter()).unwrap();
    let rec = rows.create(&mut tx, vec![vals]).await.unwrap().ids()[0];

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());

    let recs = tx.env().records(MODEL).unwrap().browse(vec![rec]).unwrap();
    let vals = recs.read(&mut tx, &["my_selection"]).await.unwrap();
    assert_eq!(vals[0]["my_selection"], Value::str("foo"));

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_035_test_write_override_selection() {
    const MODEL: &str = "test_ondelete.write_override";
    static OVERRIDE_CALLED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    OVERRIDE_CALLED.store(false, std::sync::atomic::Ordering::SeqCst);
    // Write hook on my_selection that always returns an error to simulate failing write override
    let write_hook = WriteHook::new(MODEL, const { &["my_selection"] }, |_tx, _batch| {
        OVERRIDE_CALLED.store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move { Err(Error::user("simulated write override failure")) })
    });

    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Write override model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("my_selection", &[("foo", "Foo"), ("bar", "Bar")])
                            .required(true)
                            .default(StaticValue::Str("foo")),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "my_selection",
                    &[("divinity", Some("Divinity"))],
                    &[("divinity", SelectionOnDeleteDecl::SetDefault)],
                )]
            },
        ),
    ])
    .write_hook(write_hook)
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();

    // Insert record with direct SQL (write hook only runs on write/update through ORM)
    let sql = format!(
        "INSERT INTO \"{}\" (\"name\", \"my_selection\") VALUES ('initial', 'divinity') RETURNING \"id\"",
        MODEL.replace('.', "_")
    );
    let row = tx.query(&sql, &[]).await.unwrap();
    let rec: i64 = row[0].get(0);

    // Verify initial value
    let recs = tx.env().records(MODEL).unwrap().browse(vec![rec]).unwrap();
    let vals = recs.read(&mut tx, &["my_selection"]).await.unwrap();
    assert_eq!(vals[0]["my_selection"], Value::str("divinity"));

    // Purging mod_ext triggers safe_write: ORM write fails due to write hook, direct SQL update succeeds
    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert!(
        OVERRIDE_CALLED.load(std::sync::atomic::Ordering::SeqCst),
        "safe_write must attempt ORM write first before fallback"
    );

    let recs = tx.env().records(MODEL).unwrap().browse(vec![rec]).unwrap();
    let vals = recs.read(&mut tx, &["my_selection"]).await.unwrap();
    assert_eq!(vals[0]["my_selection"], Value::str("foo"));

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_035_test_ondelete_company_dependent_multicompany() {
    const MODEL: &str = "test_ondelete.comp_dep";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Company dependent selection")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection(
                            "my_selection",
                            &[("manual", "Manual"), ("auto", "Auto")],
                        )
                        .company_dependent(true),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add(
                    "my_selection",
                    &[("semi_auto", Some("Semi Auto"))],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let table = MODEL.replace('.', "_");

    // Insert 3 records with per-company JSONB values
    // r1: c1="manual", c2="semi_auto"
    // r2: c1="auto", c2="semi_auto"
    // r3: c1="semi_auto", c2="manual"
    let sql = format!(
        "INSERT INTO \"{table}\" (\"name\", \"my_selection\") VALUES \
         ('r1', '{{\"1\": \"manual\", \"2\": \"semi_auto\"}}'::jsonb), \
         ('r2', '{{\"1\": \"auto\", \"2\": \"semi_auto\"}}'::jsonb), \
         ('r3', '{{\"1\": \"semi_auto\", \"2\": \"manual\"}}'::jsonb) \
         RETURNING \"id\""
    );
    let rows = tx.query(&sql, &[]).await.unwrap();
    let r1: i64 = rows[0].get(0);
    let r2: i64 = rows[1].get(0);
    let r3: i64 = rows[2].get(0);

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());

    // Query JSONB columns directly
    let check_sql = format!(
        "SELECT \"id\", \"my_selection\"::text FROM \"{table}\" WHERE \"id\" = ANY($1) ORDER BY \"id\""
    );
    let check_rows = tx.query(&check_sql, &[&vec![r1, r2, r3]]).await.unwrap();

    let get_json = |idx: usize| -> serde_json::Value {
        let text: String = check_rows[idx].get(1);
        serde_json::from_str(&text).unwrap()
    };

    let j1 = get_json(0);
    let j2 = get_json(1);
    let j3 = get_json(2);

    // Verify company 1 and company 2 values
    assert_eq!(j1["1"], serde_json::Value::String("manual".to_owned()));
    assert_eq!(j1["2"], serde_json::Value::Null);

    assert_eq!(j2["1"], serde_json::Value::String("auto".to_owned()));
    assert_eq!(j2["2"], serde_json::Value::Null);

    assert_eq!(j3["1"], serde_json::Value::Null);
    assert_eq!(j3["2"], serde_json::Value::String("manual".to_owned()));

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Complete `ondelete` policy suite when selection values are removed:
/// - `set default` resets to the field's default ('foo')
/// - `set null` resets to null / False
/// - `cascade` deletes the record and its external ID via unlink hook
/// - `set bar` resets to literal value 'bar'
/// - callable callback sets 'foo' and active=false
/// - base value record is unchanged
/// - company-dependent selection value is cleared across all companies in JSONB
/// - a raising write override (tested on `set null` with magic substrings) does not block cleanup
#[tokio::test]
async fn spec_field_035_all_ondelete_policies_when_value_removed() {
    const MODEL: &str = "test_ondelete.all_cases";
    static OVERRIDE_CALLED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    OVERRIDE_CALLED.store(false, std::sync::atomic::Ordering::SeqCst);

    let write_hook = WriteHook::new(MODEL, const { &["state"] }, |_tx, batch| {
        for row in batch.rows() {
            if let Some((_, Cell::Null)) = row.values().first() {
                OVERRIDE_CALLED.store(true, std::sync::atomic::Ordering::SeqCst);
                return Box::pin(async move {
                    Err(Error::user(
                        "simulated write override failure (after: check)",
                    ))
                });
            }
        }
        Box::pin(async move { Ok(()) })
    });

    let callback = SelectionCallback::new("all_cases_cb", |tx, model, ids| {
        Box::pin(async move {
            let env = tx.env();
            let rec_ids: Vec<i64> = ids.iter().map(|id| id.get()).collect();
            let records = env.records(model)?.browse(rec_ids)?;
            let mut map = std::collections::BTreeMap::new();
            map.insert("state".to_owned(), Value::str("foo"));
            map.insert("active".to_owned(), Value::Bool(false));
            records.write_values(tx, &map).await?;
            Ok(())
        })
    });

    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("All ondelete cases model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::boolean("active").default(StaticValue::Bool(true)),
                        FieldDecl::selection(
                            "state",
                            &[("base_val", "Base"), ("foo", "Foo"), ("bar", "Bar")],
                        )
                        .default(StaticValue::Str("foo")),
                        FieldDecl::selection("comp_sel", &[("c_base", "CBase")])
                            .company_dependent(true),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[
                    FieldDecl::selection_add_ondelete(
                        "state",
                        &[
                            ("v_default", Some("VDefault")),
                            ("v_null", Some("VNull")),
                            ("v_cascade", Some("VCascade")),
                            ("v_bar", Some("VBar")),
                            ("v_cb", Some("VCb")),
                        ],
                        &[
                            ("v_default", SelectionOnDeleteDecl::SetDefault),
                            ("v_null", SelectionOnDeleteDecl::SetNull),
                            ("v_cascade", SelectionOnDeleteDecl::Cascade),
                            ("v_bar", SelectionOnDeleteDecl::SetValue("bar")),
                            ("v_cb", SelectionOnDeleteDecl::Callback("all_cases_cb")),
                        ],
                    ),
                    FieldDecl::selection_add_ondelete(
                        "comp_sel",
                        &[("v_comp", Some("VComp"))],
                        &[("v_comp", SelectionOnDeleteDecl::SetNull)],
                    ),
                ]
            },
        ),
    ])
    .selection_callback(callback)
    .write_hook(write_hook)
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().with_company(1).unwrap().records(MODEL).unwrap();

    let make_vals = |name: &str, state: &str| {
        let values: erp_sdk::ValueMap = [("name", Value::str(name)), ("state", Value::str(state))]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        rows.vals_from_values(values.iter()).unwrap()
    };

    let r_default = rows
        .create(&mut tx, vec![make_vals("r_default", "v_default")])
        .await
        .unwrap()
        .ids()[0];
    let r_null = rows
        .create(&mut tx, vec![make_vals("r_null", "v_null")])
        .await
        .unwrap()
        .ids()[0];
    let r_cascade = rows
        .create(&mut tx, vec![make_vals("r_cascade", "v_cascade")])
        .await
        .unwrap()
        .ids()[0];
    bind(&mut tx, MODEL, &[("mod_ext.r_cascade", r_cascade)]).await;
    assert!(has_xid(&mut tx, "mod_ext.r_cascade").await);

    let r_bar = rows
        .create(&mut tx, vec![make_vals("r_bar", "v_bar")])
        .await
        .unwrap()
        .ids()[0];
    let r_cb = rows
        .create(&mut tx, vec![make_vals("r_cb", "v_cb")])
        .await
        .unwrap()
        .ids()[0];
    let r_base = rows
        .create(&mut tx, vec![make_vals("r_base", "base_val")])
        .await
        .unwrap()
        .ids()[0];

    // Company-dependent record with v_comp in companies 1 and 2
    let table = MODEL.replace('.', "_");
    let sql = format!(
        "INSERT INTO \"{table}\" (\"name\", \"state\", \"comp_sel\") VALUES \
         ('r_comp', 'base_val', '{{\"1\": \"v_comp\", \"2\": \"v_comp\"}}'::jsonb) \
         RETURNING \"id\""
    );
    let r_comp: i64 = tx.query(&sql, &[]).await.unwrap()[0].get(0);

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert_eq!(report.deleted, vec![(MODEL.to_owned(), 1)]);
    assert!(OVERRIDE_CALLED.load(std::sync::atomic::Ordering::SeqCst));

    // Check cascade record and xid deleted
    assert!(alive(&mut tx, MODEL, &[r_cascade]).await.is_empty());
    assert!(!has_xid(&mut tx, "mod_ext.r_cascade").await);

    // Read back remaining records
    let check_recs = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![r_default, r_null, r_bar, r_cb, r_base, r_comp])
        .unwrap();
    let vals = check_recs
        .read(&mut tx, &["state", "active"])
        .await
        .unwrap();

    // r_default: reset to default "foo"
    assert_eq!(vals[0]["state"], Value::str("foo"));
    assert_eq!(vals[0]["active"], Value::Bool(true));

    // r_null: reset to null (Value::False)
    assert_eq!(vals[1]["state"], Value::False);
    assert_eq!(vals[1]["active"], Value::Bool(true));

    // r_bar: reset to literal "bar"
    assert_eq!(vals[2]["state"], Value::str("bar"));
    assert_eq!(vals[2]["active"], Value::Bool(true));

    // r_cb: callback set active=false and state="foo"
    assert_eq!(vals[3]["state"], Value::str("foo"));
    assert_eq!(vals[3]["active"], Value::Bool(false));

    // r_base: unchanged
    assert_eq!(vals[4]["state"], Value::str("base_val"));
    assert_eq!(vals[4]["active"], Value::Bool(true));

    // r_comp: companies 1 and 2 are both cleared. With no fallback a cleared value equals the
    // fallback, so the runtime drops each key and the emptied map is SQL NULL (company-dependent
    // runtime plan: fallback-equal normalization); every company reads it as null.
    let check_sql = format!("SELECT \"comp_sel\" IS NULL FROM \"{table}\" WHERE \"id\" = $1");
    let cleared: bool = tx.query(&check_sql, &[&r_comp]).await.unwrap()[0].get(0);
    assert!(cleared);
    for company in [1, 2] {
        let record = tx
            .env()
            .with_company(company)
            .unwrap()
            .records(MODEL)
            .unwrap()
            .browse([r_comp])
            .unwrap();
        assert_eq!(
            record.value(&mut tx, "comp_sel").await.unwrap(),
            Value::False
        );
    }

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// (SEL-D10): Unregistered callback fails loudly with Programming error.
#[tokio::test]
async fn test_ondelete_unregistered_callback_fails_loudly() {
    const MODEL: &str = "test_ondelete.unregistered_cb";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Unregistered cb model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("my_selection", &[("foo", "Foo")]),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "my_selection",
                    &[("missing", Some("Missing"))],
                    &[("missing", SelectionOnDeleteDecl::Callback("no_such_cb"))],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records(MODEL).unwrap();
    let vals: erp_sdk::ValueMap = [
        ("name", Value::str("test")),
        ("my_selection", Value::str("missing")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    let _id = rows
        .create(&mut tx, vec![rows.vals_from_values(vals.iter()).unwrap()])
        .await
        .unwrap()
        .ids()[0];

    let err = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap_err();
    let erp_data::load::LoadErrorKind::Orm(orm) = err.kind() else {
        panic!("expected LoadErrorKind::Orm, got {err:?}");
    };
    assert_eq!(orm.kind(), erp_fields::ErrorKind::Programming);
    assert!(
        orm.message()
            .contains("selection ondelete callback 'no_such_cb' is not registered on the ORM"),
        "actual error: {orm:?}"
    );

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// (SEL-D10): A selection value also declared by a staying module is NOT removed.
#[tokio::test]
async fn test_ondelete_value_kept_by_staying_module() {
    const MODEL: &str = "test_ondelete.kept_val";
    static MOD_EXT2: AddonDescriptor = AddonDescriptor {
        name: "mod_ext2",
        depends: &["mod_base"],
        exact_only: false,
    };

    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
        &MOD_EXT2,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Kept value model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("state", &[("v1", "V1")])
                            .default(StaticValue::Str("v1")),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "state",
                    &[("v2", Some("V2"))],
                    &[("v2", SelectionOnDeleteDecl::Cascade)],
                )]
            },
        ),
        ModelDecl::extension("mod_ext2", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "state",
                    &[("v2", Some("V2"))],
                    &[("v2", SelectionOnDeleteDecl::SetDefault)],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records(MODEL).unwrap();
    let vals: erp_sdk::ValueMap = [("name", Value::str("rec")), ("state", Value::str("v2"))]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    let rec_id = rows
        .create(&mut tx, vec![rows.vals_from_values(vals.iter()).unwrap()])
        .await
        .unwrap()
        .ids()[0];

    // Purging only mod_ext: v2 is kept by staying module mod_ext2, so record is NOT cascaded!
    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert!(report.deleted.is_empty());

    assert_eq!(alive(&mut tx, MODEL, &[rec_id]).await, vec![rec_id]);
    let vals = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse(vec![rec_id])
        .unwrap()
        .read(&mut tx, &["state"])
        .await
        .unwrap();
    assert_eq!(vals[0]["state"], Value::str("v2"));

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// (SEL-D10): Company-dependent cascade deletion.
#[tokio::test]
async fn test_ondelete_company_dependent_cascade() {
    const MODEL: &str = "test_ondelete.comp_dep_cascade";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("CD Cascade model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("state", &[("base", "Base")]).company_dependent(true),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "state",
                    &[("rem", Some("Rem"))],
                    &[("rem", SelectionOnDeleteDecl::Cascade)],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let table = MODEL.replace('.', "_");
    let sql = format!(
        "INSERT INTO \"{table}\" (\"name\", \"state\") VALUES \
         ('r_casc', '{{\"1\": \"rem\"}}'::jsonb) RETURNING \"id\""
    );
    let r_id: i64 = tx.query(&sql, &[]).await.unwrap()[0].get(0);
    bind(&mut tx, MODEL, &[("mod_ext.r_casc", r_id)]).await;
    assert!(has_xid(&mut tx, "mod_ext.r_casc").await);

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert_eq!(report.deleted, vec![(MODEL.to_owned(), 1)]);
    assert!(alive(&mut tx, MODEL, &[r_id]).await.is_empty());
    assert!(!has_xid(&mut tx, "mod_ext.r_casc").await);

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// (SEL-D10, SEL-CLOSE-F2): Company-dependent callback policy across multiple companies.
/// Verifies:
/// 1. The callback is executed with the selected company's environment (`tx.env().company_id()`),
///    preserving system authority.
/// 2. The callback performs an ORM write (`records.write_values`) under that company environment.
/// 3. The callback cleans up the current company's JSONB key while leaving other-company values intact.
/// 4. Pending writes from company 1's callback are flushed and affect company 2's subsequent lookup.
/// 5. Caller's original environment is restored after dispatch.
#[tokio::test]
async fn test_ondelete_company_dependent_callback() {
    const MODEL: &str = "test_ondelete.comp_dep_cb";
    static CB_COMPANIES: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());
    CB_COMPANIES.lock().unwrap().clear();

    let callback = SelectionCallback::new("cd_cb", |tx, model, ids| {
        Box::pin(async move {
            let cid = tx
                .env()
                .company_id()
                .expect("env check ok")
                .expect("must have active company in company-dependent callback");
            CB_COMPANIES.lock().unwrap().push(cid);
            assert_eq!(tx.env().uid(), 1, "system authority preserved");

            let env = tx.env();
            let rec_ids: Vec<i64> = ids.iter().map(|id| id.get()).collect();
            let records = env.records(model)?.browse(rec_ids.clone())?;

            // Perform ORM write with this company's environment
            let mut write_vals = std::collections::BTreeMap::new();
            write_vals.insert(
                "name".to_owned(),
                Value::str(format!("cleaned_by_cb_co_{cid}")),
            );
            records.write_values(tx, &write_vals).await?;

            let table = model.replace('.', "_");
            let cid_str = cid.to_string();

            // Clean up current company's state in JSONB
            let clean_sql = format!(
                "UPDATE \"{table}\" SET \"state\" = jsonb_set(COALESCE(\"state\", '{{}}'::jsonb), ARRAY[$1], '\"base\"'::jsonb, true) WHERE \"id\" = ANY($2)"
            );
            tx.execute(&clean_sql, &[&cid_str, &rec_ids]).await?;

            // If processing company 1: modify r2's company 2 key in JSONB from "rem" to "manual",
            // which tests that company 1's writes are flushed and affect company 2's subsequent lookup.
            if cid == 1 {
                let mod_co2_sql = format!(
                    "UPDATE \"{table}\" SET \"state\" = jsonb_set(\"state\", ARRAY['2'], '\"manual\"'::jsonb, true) WHERE \"name\" LIKE '%r2%'"
                );
                tx.execute(&mod_co2_sql, &[]).await?;
            }

            Ok(())
        })
    });

    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("CD Callback model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("state", &[("base", "Base"), ("manual", "Manual")])
                            .company_dependent(true),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "state",
                    &[("rem", Some("Rem"))],
                    &[("rem", SelectionOnDeleteDecl::Callback("cd_cb"))],
                )]
            },
        ),
    ])
    .selection_callback(callback)
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let caller_env_before = tx.env().company_id().unwrap();
    assert!(caller_env_before.is_none());

    let table = MODEL.replace('.', "_");
    // r1: company 1 has "rem", company 2 has "manual" (kept)
    // r2: company 1 has "rem", company 2 has "rem" (to be modified by co 1 callback before co 2 lookup)
    // r3: company 1 has "manual", company 2 has "rem" (to be processed when co 2 lookup runs)
    let sql = format!(
        "INSERT INTO \"{table}\" (\"name\", \"state\") VALUES \
         ('r1_cb', '{{\"1\": \"rem\", \"2\": \"manual\"}}'::jsonb), \
         ('r2_cb', '{{\"1\": \"rem\", \"2\": \"rem\"}}'::jsonb), \
         ('r3_cb', '{{\"1\": \"manual\", \"2\": \"rem\"}}'::jsonb) \
         RETURNING \"id\""
    );
    let rows = tx.query(&sql, &[]).await.unwrap();
    let r1: i64 = rows[0].get(0);
    let r2: i64 = rows[1].get(0);
    let r3: i64 = rows[2].get(0);

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());

    // Verify company callbacks ran: company 1 for [r1, r2], then company 2 for [r3]
    // (r2 was changed to "manual" for company 2 by company 1's callback, and the flush ensured
    // company 2's lookup saw that change and omitted r2)
    let companies_called = CB_COMPANIES.lock().unwrap().clone();
    assert_eq!(companies_called, vec![1, 2]);

    // Caller environment preserved
    assert_eq!(tx.env().company_id().unwrap(), None);

    // Verify database state:
    // r1: company 1 = "base", company 2 = "manual" (unchanged other-company value)
    let res1 = tx
        .query(
            &format!("SELECT \"name\", \"state\"::text FROM \"{table}\" WHERE \"id\" = $1"),
            &[&r1],
        )
        .await
        .unwrap();
    let name1: String = res1[0].get(0);
    let state1: String = res1[0].get(1);
    let j1: serde_json::Value = serde_json::from_str(&state1).unwrap();
    assert_eq!(name1, "cleaned_by_cb_co_1");
    assert_eq!(j1["1"], serde_json::Value::String("base".to_owned()));
    assert_eq!(j1["2"], serde_json::Value::String("manual".to_owned()));

    // r2: company 1 = "base", company 2 = "manual" (modified during co 1 callback)
    let res2 = tx
        .query(
            &format!("SELECT \"name\", \"state\"::text FROM \"{table}\" WHERE \"id\" = $1"),
            &[&r2],
        )
        .await
        .unwrap();
    let name2: String = res2[0].get(0);
    let state2: String = res2[0].get(1);
    let j2: serde_json::Value = serde_json::from_str(&state2).unwrap();
    assert_eq!(name2, "cleaned_by_cb_co_1");
    assert_eq!(j2["1"], serde_json::Value::String("base".to_owned()));
    assert_eq!(j2["2"], serde_json::Value::String("manual".to_owned()));

    // r3: company 1 = "manual", company 2 = "base" (cleaned by co 2 callback)
    let res3 = tx
        .query(
            &format!("SELECT \"name\", \"state\"::text FROM \"{table}\" WHERE \"id\" = $1"),
            &[&r3],
        )
        .await
        .unwrap();
    let name3: String = res3[0].get(0);
    let state3: String = res3[0].get(1);
    let j3: serde_json::Value = serde_json::from_str(&state3).unwrap();
    assert_eq!(name3, "cleaned_by_cb_co_2");
    assert_eq!(j3["1"], serde_json::Value::String("manual".to_owned()));
    assert_eq!(j3["2"], serde_json::Value::String("base".to_owned()));

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// (SEL-CLOSE-F1): An ordinary write override returning magic message substrings
/// ("(after: input)", "the transaction was cancelled") rolls back cleanly inside savepoint and
/// falls back to direct SQL update.
#[tokio::test]
async fn test_ondelete_write_override_with_magic_substring_falls_back_cleanly() {
    const MODEL: &str = "test_ondelete.magic_msg";
    static OVERRIDE_RAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    OVERRIDE_RAN.store(false, std::sync::atomic::Ordering::SeqCst);

    let write_hook = WriteHook::new(MODEL, const { &["status"] }, |_tx, _batch| {
        OVERRIDE_RAN.store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            Err(Error::user(
                "override failed (after: input) while the transaction was cancelled by user policy",
            ))
        })
    });

    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Magic msg model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("status", &[("foo", "Foo")]),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "status",
                    &[("rem", Some("Rem"))],
                    &[("rem", SelectionOnDeleteDecl::SetNull)],
                )]
            },
        ),
    ])
    .write_hook(write_hook)
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let table = MODEL.replace('.', "_");
    let sql = format!(
        "INSERT INTO \"{table}\" (\"name\", \"status\") VALUES ('r_magic', 'rem') RETURNING \"id\""
    );
    let r_id: i64 = tx.query(&sql, &[]).await.unwrap()[0].get(0);

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert!(OVERRIDE_RAN.load(std::sync::atomic::Ordering::SeqCst));

    let check = tx
        .query(
            &format!("SELECT \"status\" FROM \"{table}\" WHERE \"id\" = $1"),
            &[&r_id],
        )
        .await
        .unwrap();
    let val: Option<String> = check[0].get(0);
    assert_eq!(val, None, "safe_write fallback updated field to NULL");

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// (SEL-CLOSE-F1): Pre-existing transaction poison prevents SQL fallback.
#[tokio::test]
async fn test_ondelete_pre_existing_poison_does_not_execute_sql_fallback() {
    const MODEL: &str = "test_ondelete.poison_boundary";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Poison boundary model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("status", &[("foo", "Foo")]),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "status",
                    &[("rem", Some("Rem"))],
                    &[("rem", SelectionOnDeleteDecl::SetNull)],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let table = MODEL.replace('.', "_");
    let sql = format!(
        "INSERT INTO \"{table}\" (\"name\", \"status\") VALUES ('r_poison', 'rem') RETURNING \"id\""
    );
    let _r_id: i64 = tx.query(&sql, &[]).await.unwrap()[0].get(0);

    // Poison transaction upfront
    tx.poison_for_tests(&Error::validation("pre-existing transaction poison"));
    assert!(tx.is_poisoned());

    let err = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap_err();
    assert_eq!(err.orm_kind(), Some(erp_fields::ErrorKind::Validation));
    match err.kind() {
        erp_data::load::LoadErrorKind::Orm(e) => {
            assert!(
                e.message().contains("pre-existing transaction poison"),
                "actual: {}",
                e.message()
            );
        }
        other => panic!("expected Orm error, got {other:?}"),
    }

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// (SEL-CLOSE-F1): Cancelled transaction prevents SQL fallback.
#[tokio::test]
async fn test_ondelete_cancelled_transaction_does_not_execute_sql_fallback() {
    const MODEL: &str = "test_ondelete.cancel_boundary";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("Cancel boundary model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("status", &[("foo", "Foo")]),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "status",
                    &[("rem", Some("Rem"))],
                    &[("rem", SelectionOnDeleteDecl::SetNull)],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let table = MODEL.replace('.', "_");
    let sql = format!(
        "INSERT INTO \"{table}\" (\"name\", \"status\") VALUES ('r_cancel', 'rem') RETURNING \"id\""
    );
    let _r_id: i64 = tx.query(&sql, &[]).await.unwrap()[0].get(0);

    // Cancel transaction upfront
    tx.taint_cancelled_for_tests();

    let err = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap_err();
    assert_eq!(err.orm_kind(), Some(erp_fields::ErrorKind::Programming));
    match err.kind() {
        erp_data::load::LoadErrorKind::Orm(e) => {
            assert!(
                e.message().contains("an ORM operation was cancelled"),
                "actual: {}",
                e.message()
            );
        }
        other => panic!("expected Orm error, got {other:?}"),
    }

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// (SEL-D10, SEL-D12): Company-dependent set default policy.
#[tokio::test]
async fn test_ondelete_company_dependent_default() {
    const MODEL: &str = "test_ondelete.comp_dep_def";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("CD Default model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("state", &[("foo", "Foo"), ("bar", "Bar")])
                            .company_dependent(true)
                            .default(StaticValue::Str("foo")),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "state",
                    &[("rem", Some("Rem"))],
                    &[("rem", SelectionOnDeleteDecl::SetDefault)],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let table = MODEL.replace('.', "_");
    let sql = format!(
        "INSERT INTO \"{table}\" (\"name\", \"state\") VALUES \
         ('r_def', '{{\"1\": \"rem\"}}'::jsonb) RETURNING \"id\""
    );
    let r_id: i64 = tx.query(&sql, &[]).await.unwrap()[0].get(0);

    let report = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());

    let check_sql = format!("SELECT \"state\"::text FROM \"{table}\" WHERE \"id\" = $1");
    let text: String = tx.query(&check_sql, &[&r_id]).await.unwrap()[0].get(0);
    let j: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(j["1"], serde_json::Value::String("foo".to_owned()));

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// (SEL-D7): Malformed JSONB in company-dependent field fails loudly.
#[tokio::test]
async fn test_ondelete_company_dependent_malformed_json_fails_loudly() {
    const MODEL: &str = "test_ondelete.comp_dep_malformed";
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &model_data::ADDON,
        &MOD_BASE,
        &MOD_EXT,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("mod_base", MODEL)
            .description("CD Malformed model")
            .fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("state", &[("foo", "Foo")]).company_dependent(true),
                    ]
                },
            ),
        ModelDecl::extension("mod_ext", MODEL, &[MODEL]).fields(
            const {
                &[FieldDecl::selection_add_ondelete(
                    "state",
                    &[("rem", Some("Rem"))],
                    &[("rem", SelectionOnDeleteDecl::SetNull)],
                )]
            },
        ),
    ])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let table = MODEL.replace('.', "_");
    // Inject non-object JSON array into the company_dependent jsonb column
    let sql = format!(
        "INSERT INTO \"{table}\" (\"name\", \"state\") VALUES \
         ('r_bad', '[\"bad\", \"array\"]'::jsonb) RETURNING \"id\""
    );
    let _r_id: i64 = tx.query(&sql, &[]).await.unwrap()[0].get(0);

    let err = purge_module_data(&mut tx, &["mod_ext"]).await.unwrap_err();
    let erp_data::load::LoadErrorKind::Orm(orm) = err.kind() else {
        panic!("expected LoadErrorKind::Orm, got {err:?}");
    };
    assert_eq!(orm.kind(), erp_fields::ErrorKind::Programming);
    assert!(
        orm.message()
            .contains("malformed JSONB in company-dependent field"),
        "actual error: {orm:?}"
    );

    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// (SEL-D8): Duplicate selection callback on OrmBuilder fails loudly at build.
#[tokio::test]
async fn test_duplicate_callback_on_orm_builder_fails() {
    let cb1 = SelectionCallback::new("dup_cb", |_tx, _model, _ids| {
        Box::pin(async move { Ok(()) })
    });
    let cb2 = SelectionCallback::new("dup_cb", |_tx, _model, _ids| {
        Box::pin(async move { Ok(()) })
    });

    let err = match Harness::builder([&erp_addon_base::ADDON])
        .selection_callback(cb1)
        .selection_callback(cb2)
        .build()
        .await
    {
        Ok(_) => panic!("expected duplicate callback error"),
        Err(HarnessError::Orm(e)) => e,
        Err(other) => panic!("unexpected error: {other:?}"),
    };
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert!(
        err.message()
            .contains("duplicate selection callback 'dup_cb' registered on OrmBuilder"),
        "actual: {}",
        err.message()
    );
}
