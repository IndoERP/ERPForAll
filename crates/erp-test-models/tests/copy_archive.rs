//! Tests for `copy`, `copy_data`, `name_create`, `action_archive`, and `action_unarchive`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_fields::{ErrorKind, IndexKind, OnDelete, Value, ValueMap};
use erp_registry::{FieldDecl, ModelDecl, StaticValue};
use erp_sdk::domain::Operator;
use erp_sdk::{Domain, Operation};
use erp_test_models::base_users;
use erp_testkit::{DomainPolicy, Harness, Otherwise};

static TEST_COPY: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
    name: "test_copy",
    depends: &["base"],
    exact_only: false,
};

async fn test_harness() -> Harness {
    harness_with_policy(erp_testkit::FieldPolicy::new().enforce_groups()).await
}

async fn harness_with_policy(policy: impl erp_sdk::AccessPolicy + 'static) -> Harness {
    Harness::builder([&base_users::ADDON, &TEST_COPY])
        .policy(policy)
        .with_decls([
            // Target for many2one & many2many
            ModelDecl::new("test_copy", "test_copy.tag")
                .fields(const { &[FieldDecl::char("name")] }),
            // Line for copy=True one2many
            ModelDecl::new("test_copy", "test_copy.line").fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::many2one("main_id", "test_copy.main")
                            .ondelete(OnDelete::Cascade),
                    ]
                },
            ),
            // Line for copy=False one2many
            ModelDecl::new("test_copy", "test_copy.nocopy_line").fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::many2one("main_id", "test_copy.main")
                            .ondelete(OnDelete::Cascade),
                    ]
                },
            ),
            ModelDecl::new("test_copy", "test_copy.main").fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::selection("state", &[("draft", "Draft"), ("done", "Done")])
                            .default(StaticValue::Str("draft")),
                        FieldDecl::char("secret").copy(false),
                        FieldDecl::many2one("tag_id", "test_copy.tag"),
                        FieldDecl::one2many("line_ids", "test_copy.line", "main_id").copy(true),
                        FieldDecl::one2many("nocopy_line_ids", "test_copy.nocopy_line", "main_id")
                            .copy(false),
                        FieldDecl::many2many("tag_ids", "test_copy.tag"),
                    ]
                },
            ),
            ModelDecl::new("test_copy", "test_copy.unit")
                .fields(const { &[FieldDecl::char("name"), FieldDecl::char("unit_code")] }),
            ModelDecl::new("test_copy", "test_copy.box")
                .inherits(&[("test_copy.unit", "unit_id")])
                .fields(
                    const {
                        &[
                            FieldDecl::many2one("unit_id", "test_copy.unit")
                                .required(true)
                                .ondelete(OnDelete::Cascade),
                            FieldDecl::integer("size"),
                            FieldDecl::char("box_code"),
                        ]
                    },
                ),
            ModelDecl::new("test_copy", "test_copy.category")
                .parent_store(true)
                .parent_name("parent_id")
                .fields(
                    const {
                        &[
                            FieldDecl::char("name"),
                            FieldDecl::char("parent_path").index(Some(IndexKind::Btree)),
                            FieldDecl::many2one("parent_id", "test_copy.category")
                                .ondelete(OnDelete::Cascade),
                            FieldDecl::one2many("child_ids", "test_copy.category", "parent_id")
                                .copy(true),
                        ]
                    },
                ),
            // Self-referential cycler model for cycle guard, recursion depth, and cap tests
            ModelDecl::new("test_copy", "test_copy.cycler").fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::many2one("cycler_id", "test_copy.cycler")
                            .ondelete(OnDelete::Cascade),
                        FieldDecl::one2many("cycler_ids", "test_copy.cycler", "cycler_id")
                            .copy(true),
                    ]
                },
            ),
            // Model with group-restricted field for S7 test
            ModelDecl::new("test_copy", "test_copy.restricted").fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::char("secret")
                            .copy(true)
                            .groups("base.group_system"),
                    ]
                },
            ),
            ModelDecl::new("test_copy", "test_copy.rec")
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("test_copy", "test_copy.norec")
                .fields(const { &[FieldDecl::integer("code")] }),
            // Archive models: with and without active
            ModelDecl::new("test_copy", "test_copy.archivable").fields(
                const {
                    &[
                        FieldDecl::char("name"),
                        FieldDecl::boolean("active").default(StaticValue::Bool(true)),
                    ]
                },
            ),
            ModelDecl::new("test_copy", "test_copy.no_active")
                .fields(const { &[FieldDecl::char("name")] }),
        ])
        .build()
        .await
        .expect("harness")
}

#[tokio::test]
async fn spec_crud_037_copy_excludes_magic_non_copyable_and_delegation() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();

    let tag_model = tx.env().records("test_copy.tag").unwrap();
    let tag1 = tag_model
        .create_values(&mut tx, &[vmap(&[("name", Value::str("T1"))])])
        .await
        .unwrap();
    let tag2 = tag_model
        .create_values(&mut tx, &[vmap(&[("name", Value::str("T2"))])])
        .await
        .unwrap();

    let main_model = tx.env().records("test_copy.main").unwrap();

    let record = main_model
        .create_values(
            &mut tx,
            &[vmap(&[
                ("name", Value::str("Original")),
                ("state", Value::str("done")),
                ("secret", Value::str("shh")),
                ("tag_id", Value::Int(tag1.ids()[0])),
                (
                    "tag_ids",
                    Value::list([Value::Int(tag1.ids()[0]), Value::Int(tag2.ids()[0])]),
                ),
                (
                    "line_ids",
                    Value::list([
                        Value::list([
                            Value::Int(0),
                            Value::Int(0),
                            Value::Map(Arc::new(val_map(&[("name", Value::str("L1"))]))),
                        ]),
                        Value::list([
                            Value::Int(0),
                            Value::Int(0),
                            Value::Map(Arc::new(val_map(&[("name", Value::str("L2"))]))),
                        ]),
                    ]),
                ),
                (
                    "nocopy_line_ids",
                    Value::list([Value::list([
                        Value::Int(0),
                        Value::Int(0),
                        Value::Map(Arc::new(val_map(&[("name", Value::str("NC1"))]))),
                    ])]),
                ),
            ])],
        )
        .await
        .unwrap();

    // 1. copy() without default
    let copy1 = record.copy(&mut tx, None).await.unwrap();
    assert_ne!(copy1.ids()[0], record.ids()[0]);

    let name1 = copy1.value(&mut tx, "name").await.unwrap();
    assert_eq!(name1, Value::str("Original"));

    // State is reset to default ("draft")
    let state1 = copy1.value(&mut tx, "state").await.unwrap();
    assert_eq!(state1, Value::str("draft"));

    // copy=false Char field is empty
    let secret1 = copy1.value(&mut tx, "secret").await.unwrap();
    assert_eq!(secret1, Value::False);

    // many2one is preserved
    let tag_id1 = copy1.value(&mut tx, "tag_id").await.unwrap();
    assert_eq!(tag_id1, Value::Int(tag1.ids()[0]));

    // many2many tags are preserved
    let tag_ids1 = copy1.values(&mut tx, "tag_ids").await.unwrap();
    assert_eq!(
        tag_ids1[0],
        Value::list([Value::Int(tag1.ids()[0]), Value::Int(tag2.ids()[0])])
    );

    // one2many with copy=True has 2 lines in id order
    let lines1_vals = copy1.values(&mut tx, "line_ids").await.unwrap();
    let lines1_ids = match &lines1_vals[0] {
        Value::List(ids) => ids
            .iter()
            .map(|v| match v {
                Value::Int(i) => *i,
                _ => panic!("expected Value::Int"),
            })
            .collect::<Vec<_>>(),
        _ => panic!("expected Value::List"),
    };
    assert_eq!(lines1_ids.len(), 2);

    let line_model = tx.env().records("test_copy.line").unwrap();
    let lines1_recs = line_model.browse(lines1_ids).unwrap();
    let line_names = lines1_recs.values(&mut tx, "name").await.unwrap();
    assert_eq!(line_names, [Value::str("L1"), Value::str("L2")]);

    // one2many without copy=True has no lines
    let nocopy_vals = copy1.values(&mut tx, "nocopy_line_ids").await.unwrap();
    let nocopy_ids = match &nocopy_vals[0] {
        Value::List(ids) => ids.clone(),
        _ => panic!("expected Value::List"),
    };
    assert_eq!(nocopy_ids.len(), 0);

    // 2. copy({'name': 'N'})
    let mut default_map = ValueMap::new();
    default_map.insert("name".to_owned(), Value::str("N"));
    let copy2 = record.copy(&mut tx, Some(default_map)).await.unwrap();
    let name2 = copy2.value(&mut tx, "name").await.unwrap();
    assert_eq!(name2, Value::str("N"));

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_038_copy_delegation() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();

    let unit_model = tx.env().records("test_copy.unit").unwrap();
    let box_model = tx.env().records("test_copy.box").unwrap();

    let u = unit_model
        .create_values(
            &mut tx,
            &[vmap(&[
                ("name", Value::str("u")),
                ("unit_code", Value::str("UC1")),
            ])],
        )
        .await
        .unwrap();

    let b = box_model
        .create_values(
            &mut tx,
            &[vmap(&[
                ("unit_id", Value::Int(u.ids()[0])),
                ("size", Value::Int(10)),
                ("box_code", Value::str("B1")),
            ])],
        )
        .await
        .unwrap();

    // 1. B.copy()
    let b_copy1 = b.copy(&mut tx, None).await.unwrap();
    assert_ne!(b_copy1.ids()[0], b.ids()[0]);

    // Creates a new unit
    let new_unit_id = b_copy1.value(&mut tx, "unit_id").await.unwrap();
    assert_ne!(new_unit_id, Value::Int(u.ids()[0]));

    // Inherited values are copied
    let new_name = b_copy1.value(&mut tx, "name").await.unwrap();
    assert_eq!(new_name, Value::str("u"));
    let new_uc = b_copy1.value(&mut tx, "unit_code").await.unwrap();
    assert_eq!(new_uc, Value::str("UC1"));

    // 2. B.copy({'unit_id': U2.id})
    let u2 = unit_model
        .create_values(
            &mut tx,
            &[vmap(&[
                ("name", Value::str("u2")),
                ("unit_code", Value::str("UC2")),
            ])],
        )
        .await
        .unwrap();

    let mut default = ValueMap::new();
    default.insert("unit_id".to_owned(), Value::Int(u2.ids()[0]));
    let units_before = unit_model
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap()
        .len();
    let b_copy2 = b.copy(&mut tx, Some(default)).await.unwrap();
    assert_eq!(
        unit_model
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .len(),
        units_before
    );

    // Reuses U2
    let b2_unit_id = b_copy2.value(&mut tx, "unit_id").await.unwrap();
    assert_eq!(b2_unit_id, Value::Int(u2.ids()[0]));

    // Inherited fields from U2 are not overwritten
    let b2_name = b_copy2.value(&mut tx, "name").await.unwrap();
    assert_eq!(b2_name, Value::str("u2"));
    let b2_uc = b_copy2.value(&mut tx, "unit_code").await.unwrap();
    assert_eq!(b2_uc, Value::str("UC2"));

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_039_copy_of_hierarchies_and_cycles() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();

    let cat_model = tx.env().records("test_copy.category").unwrap();

    // Root -> Child1, Child2, Child1 -> GrandChild
    let root = cat_model
        .create_values(&mut tx, &[vmap(&[("name", Value::str("Root"))])])
        .await
        .unwrap();

    let c1 = cat_model
        .create_values(
            &mut tx,
            &[vmap(&[
                ("name", Value::str("C1")),
                ("parent_id", Value::Int(root.ids()[0])),
            ])],
        )
        .await
        .unwrap();

    let _c2 = cat_model
        .create_values(
            &mut tx,
            &[vmap(&[
                ("name", Value::str("C2")),
                ("parent_id", Value::Int(root.ids()[0])),
            ])],
        )
        .await
        .unwrap();

    let _gc = cat_model
        .create_values(
            &mut tx,
            &[vmap(&[
                ("name", Value::str("GC")),
                ("parent_id", Value::Int(c1.ids()[0])),
            ])],
        )
        .await
        .unwrap();

    // Total 4 nodes
    assert_eq!(
        cat_model
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .len(),
        4
    );

    tx.invalidate_all().await.unwrap();
    // Cold four-node hierarchy: observed ceiling in evidence/m3-rpc-j2-copy/focused.txt.
    tx.capture_statements(true);
    let queries_before = tx.query_count();
    let data = root.copy_data(&mut tx, None).await.unwrap();
    let copy_queries = tx.query_count() - queries_before;
    eprintln!(
        "copy_data two-level tree queries={copy_queries}; statements={:?}",
        tx.captured()
    );
    assert_eq!(data.len(), 1);
    assert!(
        copy_queries <= 4,
        "copy_data used {copy_queries} statements: {:?}",
        tx.captured()
    );
    tx.capture_statements(false);

    // Copy root
    let new_root = root.copy(&mut tx, None).await.unwrap();
    assert_ne!(new_root.ids()[0], root.ids()[0]);

    // Now total nodes is 8 (4 original + 4 copies)
    let total = cat_model.search(&mut tx, &Domain::TRUE).await.unwrap();
    assert_eq!(total.len(), 8);

    // parent_path of new root is freshly computed
    let root_pp = new_root.value(&mut tx, "parent_path").await.unwrap();
    assert_eq!(root_pp, Value::str(format!("{}/", new_root.ids()[0])));

    let old_children = relation_ids(&root.value(&mut tx, "child_ids").await.unwrap());
    let new_children = relation_ids(&new_root.value(&mut tx, "child_ids").await.unwrap());
    assert_eq!(new_children.len(), 2);
    assert!(new_children.iter().all(|id| !old_children.contains(id)));
    let copied_children = cat_model.browse(new_children.clone()).unwrap();
    assert_eq!(
        copied_children.values(&mut tx, "name").await.unwrap(),
        [Value::str("C1"), Value::str("C2")]
    );
    assert_eq!(
        copied_children.values(&mut tx, "parent_id").await.unwrap(),
        [Value::Int(new_root.ids()[0]), Value::Int(new_root.ids()[0])]
    );
    let grandchildren = relation_ids(
        &cat_model
            .browse([new_children[0]])
            .unwrap()
            .value(&mut tx, "child_ids")
            .await
            .unwrap(),
    );
    assert_eq!(grandchildren.len(), 1);
    assert_ne!(grandchildren[0], _gc.ids()[0]);
    let grandchild = cat_model.browse(grandchildren).unwrap();
    assert_eq!(
        grandchild.value(&mut tx, "name").await.unwrap(),
        Value::str("GC")
    );
    assert_eq!(
        grandchild.value(&mut tx, "parent_id").await.unwrap(),
        Value::Int(new_children[0])
    );
    assert_eq!(
        grandchild.value(&mut tx, "parent_path").await.unwrap(),
        Value::str(format!(
            "{}/{}/{}/",
            new_root.ids()[0],
            new_children[0],
            grandchild.ids()[0]
        ))
    );

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn copy_circular_relation_guard() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();

    let cycler_model = tx.env().records("test_copy.cycler").unwrap();
    let r1 = cycler_model
        .create_values(&mut tx, &[vmap(&[("name", Value::str("R1"))])])
        .await
        .unwrap();
    let r2 = cycler_model
        .create_values(
            &mut tx,
            &[vmap(&[
                ("name", Value::str("R2")),
                ("cycler_id", Value::Int(r1.ids()[0])),
            ])],
        )
        .await
        .unwrap();

    // Make cycle: R1 points to R2, and R2 points to R1
    let mut update = ValueMap::new();
    update.insert("cycler_id".to_owned(), Value::Int(r2.ids()[0]));
    r1.write_values(&mut tx, update.iter()).await.unwrap();

    // copy_data on R1 terminates safely, cycle back to R1 produces None and is omitted
    let cd = r1.copy_data(&mut tx, None).await.unwrap();
    assert_eq!(cd.len(), 1);
    let map = cd[0].as_ref().unwrap();
    assert_eq!(map.get("name"), Some(&Value::str("R1")));
    let cmds = match map.get("cycler_ids") {
        Some(Value::List(l)) => l.clone(),
        _ => panic!("expected Value::List"),
    };
    assert_eq!(cmds.len(), 1);

    // copy() on R1 succeeds without infinite loop
    let copied = r1.copy(&mut tx, None).await.unwrap();
    assert_ne!(copied.ids()[0], r1.ids()[0]);

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn copy_fails_if_record_already_copied_in_chain() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();

    let rec_model = tx.env().records("test_copy.rec").unwrap();
    let r = rec_model
        .create_values(&mut tx, &[vmap(&[("name", Value::str("Single"))])])
        .await
        .unwrap();

    let r_id = r.ids()[0];
    let dup_records = rec_model.browse([r_id, r_id]).unwrap();
    let err = dup_records.copy(&mut tx, None).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.to_string(),
        format!("copy: record test_copy.rec,{r_id} already copied in this chain")
    );

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn copy_depth_limit() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();

    let cycler_model = tx.env().records("test_copy.cycler").unwrap();
    let mut root_rec = None;
    let mut current_parent: Option<i64> = None;

    // Create 18 records deep: child_ids chained
    for i in 0..18 {
        let mut vals = vmap(&[("name", Value::str(format!("Node{i}")))]);
        if let Some(pid) = current_parent {
            vals.insert("cycler_id".to_owned(), Value::Int(pid));
        }
        let rec = cycler_model.create_values(&mut tx, &[vals]).await.unwrap();
        let rec_id = rec.ids()[0];
        if root_rec.is_none() {
            root_rec = Some(rec);
        }
        current_parent = Some(rec_id);
    }

    let root = root_rec.unwrap();
    let err = root.copy_data(&mut tx, None).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.to_string(), "copy: nested deeper than 16 levels");

    // Revisit the root that failed after entering the seen set; explicit empty children
    // avoid repeating the depth refusal while proving root tracker state was released.
    let defaults = val_map(&[("cycler_ids", Value::list([]))]);
    let recovered = root.copy_data(&mut tx, Some(&defaults)).await.unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(
        recovered[0].as_ref().unwrap().get("name"),
        Some(&Value::str("Node0"))
    );

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn copy_depth_boundary_does_not_charge_empty_children() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_copy.cycler").unwrap();
    let root = model
        .create_values(&mut tx, &[vmap(&[("name", Value::str("Root"))])])
        .await
        .unwrap();
    let leaf = model
        .create_values(
            &mut tx,
            &[vmap(&[
                ("name", Value::str("Leaf")),
                ("cycler_id", Value::Int(root.ids()[0])),
            ])],
        )
        .await
        .unwrap();
    tx.set_copy_limits_for_tests(1, 10_000).unwrap();
    let data = root.copy_data(&mut tx, None).await.unwrap();
    let children = match data[0].as_ref().unwrap().get("cycler_ids") {
        Some(Value::List(commands)) => commands,
        other => panic!("expected copied children, got {other:?}"),
    };
    assert_eq!(children.len(), 1);
    let child = match &children[0] {
        Value::List(command) => match &command[2] {
            Value::Map(values) => values,
            other => panic!("expected child values, got {other:?}"),
        },
        other => panic!("expected create command, got {other:?}"),
    };
    assert_eq!(child.get("name"), Some(&Value::str("Leaf")));
    assert_eq!(child.get("cycler_ids"), Some(&Value::list([])));

    model
        .create_values(
            &mut tx,
            &[vmap(&[
                ("name", Value::str("Grandchild")),
                ("cycler_id", Value::Int(leaf.ids()[0])),
            ])],
        )
        .await
        .unwrap();
    let err = root.copy_data(&mut tx, None).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "copy: nested deeper than 1 levels");
    tx.set_copy_limits_for_tests(2, 10_000).unwrap();
    let recovered = root.copy(&mut tx, None).await.unwrap();
    assert_eq!(recovered.len(), 1);
    assert_ne!(recovered.ids(), root.ids());
    assert_eq!(
        model
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        6
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn copy_data_refuses_group_restricted_field() {
    let h = test_harness().await;
    let mut admin_tx = h.begin().await.unwrap();

    let restr_model = admin_tx.env().records("test_copy.restricted").unwrap();
    let r = restr_model
        .create_values(
            &mut admin_tx,
            &[vmap(&[
                ("name", Value::str("Public")),
                ("secret", Value::str("Classified")),
            ])],
        )
        .await
        .unwrap();
    let r_id = r.ids()[0];
    admin_tx.commit().await.unwrap();

    // User 2 (demo user) lacks base.group_system
    let mut user_tx = h.begin_as(2).await.unwrap();
    let user_recs = user_tx
        .env()
        .records("test_copy.restricted")
        .unwrap()
        .browse([r_id])
        .unwrap();

    let err = user_recs.copy_data(&mut user_tx, None).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "You do not have enough rights to access the field \"secret\" on test_copy.restricted (test_copy.restricted). Please contact your system administrator.\n\nOperation: read"
    );

    user_tx.rollback().await.unwrap();
    drop(user_tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_model_016_name_create() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();

    let rec_model = tx.env().records("test_copy.rec").unwrap();
    let (id, display_name) = rec_model.name_create(&mut tx, "My Record").await.unwrap();
    assert!(id > 0);
    assert_eq!(&*display_name, "My Record");

    // Verify record exists and has name
    let found = rec_model.browse([id]).unwrap();
    let name_val = found.value(&mut tx, "name").await.unwrap();
    assert_eq!(name_val, Value::str("My Record"));

    // Model without _rec_name fails loudly
    let norec_model = tx.env().records("test_copy.norec").unwrap();
    let err = norec_model
        .name_create(&mut tx, "No Rec")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "Cannot name_create on test_copy.norec");

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_action_archive_and_unarchive() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();

    let arch_model = tx.env().records("test_copy.archivable").unwrap();
    let r1 = arch_model
        .create_values(
            &mut tx,
            &[
                vmap(&[("name", Value::str("R1")), ("active", Value::Bool(true))]),
                vmap(&[("name", Value::str("R2")), ("active", Value::Bool(false))]),
            ],
        )
        .await
        .unwrap();

    tx.flush().await.unwrap();
    // A database trigger observes every row actually updated, including redundant writes.
    tx.batch("CREATE FUNCTION reject_redundant_archive() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF OLD.active IS NOT DISTINCT FROM NEW.active THEN RAISE EXCEPTION 'redundant active write'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_redundant_archive BEFORE UPDATE ON test_copy_archivable FOR EACH ROW EXECUTE FUNCTION reject_redundant_archive();").await.unwrap();

    // action_archive sets active to false on active records
    r1.action_archive(&mut tx).await.unwrap();

    let actives = r1.values(&mut tx, "active").await.unwrap();
    assert_eq!(actives, [Value::Bool(false), Value::Bool(false)]);

    tx.flush().await.unwrap();
    tx.capture_statements(true);
    r1.action_archive(&mut tx).await.unwrap();
    tx.flush().await.unwrap();
    assert!(
        tx.captured().iter().all(|sql| !sql.starts_with("UPDATE ")),
        "{:?}",
        tx.captured()
    );
    tx.capture_statements(false);

    // action_unarchive sets active to true on inactive records
    r1.action_unarchive(&mut tx).await.unwrap();

    let actives = r1.values(&mut tx, "active").await.unwrap();
    assert_eq!(actives, [Value::Bool(true), Value::Bool(true)]);

    // Model without active field fails loudly
    let no_active_model = tx.env().records("test_copy.no_active").unwrap();
    let na = no_active_model
        .create_values(&mut tx, &[vmap(&[("name", Value::str("NA"))])])
        .await
        .unwrap();

    let err_arch = na.action_archive(&mut tx).await.unwrap_err();
    assert_eq!(err_arch.kind(), ErrorKind::Programming);
    assert_eq!(
        err_arch.message(),
        "No 'active' field on model test_copy.no_active"
    );

    let err_unarch = na.action_unarchive(&mut tx).await.unwrap_err();
    assert_eq!(err_unarch.kind(), ErrorKind::Programming);
    assert_eq!(
        err_unarch.message(),
        "No 'active' field on model test_copy.no_active"
    );

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

fn vmap(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn val_map(pairs: &[(&str, Value)]) -> ValueMap {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn relation_ids(value: &Value) -> Vec<i64> {
    match value {
        Value::List(ids) => ids
            .iter()
            .map(|id| match id {
                Value::Int(id) => *id,
                other => panic!("expected relation id, got {other:?}"),
            })
            .collect(),
        other => panic!("expected relation list, got {other:?}"),
    }
}

#[tokio::test]
async fn copy_filters_unreadable_many2many_targets() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            "test_copy.tag",
            Operation::Read,
            Domain::condition("name", Operator::Ne, "Hidden").unwrap(),
        )
        .build()
        .unwrap();
    let h = harness_with_policy(policy).await;
    let mut admin = h.begin().await.unwrap();
    admin.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'Copy user', 'copy_user', true)").await.unwrap();
    let tags = admin
        .env()
        .records("test_copy.tag")
        .unwrap()
        .create_values(
            &mut admin,
            &[
                vmap(&[("name", Value::str("First"))]),
                vmap(&[("name", Value::str("Hidden"))]),
                vmap(&[("name", Value::str("Last"))]),
            ],
        )
        .await
        .unwrap();
    let record = admin
        .env()
        .records("test_copy.main")
        .unwrap()
        .create_values(
            &mut admin,
            &[vmap(&[
                ("name", Value::str("Source")),
                (
                    "tag_ids",
                    Value::list(tags.ids().iter().copied().map(Value::Int)),
                ),
            ])],
        )
        .await
        .unwrap();
    let source_id = record.ids()[0];
    let expected = vec![tags.ids()[0], tags.ids()[2]];
    admin.commit().await.unwrap();
    let mut user = h.begin_as(2).await.unwrap();
    let source = user
        .env()
        .records("test_copy.main")
        .unwrap()
        .browse([source_id])
        .unwrap();
    let copied = source.copy(&mut user, None).await.unwrap();
    assert_eq!(
        relation_ids(&copied.value(&mut user, "tag_ids").await.unwrap()),
        expected
    );
    user.rollback().await.unwrap();
    drop(user);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn copy_record_limit_refusal_releases_tracker() {
    let h = test_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_copy.cycler").unwrap();
    let root = model
        .create_values(&mut tx, &[vmap(&[("name", Value::str("Root"))])])
        .await
        .unwrap();
    model
        .create_values(
            &mut tx,
            &[
                vmap(&[
                    ("name", Value::str("A")),
                    ("cycler_id", Value::Int(root.ids()[0])),
                ]),
                vmap(&[
                    ("name", Value::str("B")),
                    ("cycler_id", Value::Int(root.ids()[0])),
                ]),
            ],
        )
        .await
        .unwrap();
    for (depth, records) in [(0, 2), (16, 0), (17, 2), (16, 10_001)] {
        let err = tx.set_copy_limits_for_tests(depth, records).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            "test copy limits must be positive and no greater than production bounds"
        );
    }
    tx.set_copy_limits_for_tests(16, 2).unwrap();
    let err = root.copy_data(&mut tx, None).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "copy: exceeded limit of 2 records");
    tx.set_copy_limits_for_tests(16, 10_000).unwrap();
    let recovered = root.copy_data(&mut tx, None).await.unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(
        recovered[0].as_ref().unwrap().get("name"),
        Some(&Value::str("Root"))
    );
    match recovered[0].as_ref().unwrap().get("cycler_ids") {
        Some(Value::List(commands)) => assert_eq!(commands.len(), 2),
        other => panic!("expected copied children, got {other:?}"),
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Independent ORM instances share only database effects; losing one connection rolls back its copy.
#[tokio::test]
async fn copy_survives_loss_of_another_orm_instance() {
    let h = test_harness().await;
    let mut setup = h.begin().await.unwrap();
    let source = setup
        .env()
        .records("test_copy.rec")
        .unwrap()
        .create_values(&mut setup, &[vmap(&[("name", Value::str("Shared"))])])
        .await
        .unwrap();
    let source_id = source.ids()[0];
    setup.commit().await.unwrap();
    drop(setup);

    let other = erp_sdk::Orm::builder(Arc::clone(h.registry()), h.db().pool().clone())
        .policy(erp_sdk::StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut lost = h.begin().await.unwrap();
    lost.env()
        .records("test_copy.rec")
        .unwrap()
        .browse([source_id])
        .unwrap()
        .copy(&mut lost, None)
        .await
        .unwrap();
    lost.flush().await.unwrap();
    let pid = erp_testkit::backend_pid(&mut lost).await.unwrap();
    let admin = h.db().client().await.unwrap();
    let killed = admin
        .query_one("SELECT pg_terminate_backend($1)", &[&pid])
        .await
        .unwrap();
    assert!(killed.get::<_, bool>(0));
    drop(admin);
    drop(lost);

    let mut survivor = other.begin(h.principal()).await.unwrap();
    let records = survivor.env().records("test_copy.rec").unwrap();
    let copied = records
        .browse([source_id])
        .unwrap()
        .copy(&mut survivor, None)
        .await
        .unwrap();
    assert_ne!(copied.ids()[0], source_id);
    assert_eq!(
        copied.value(&mut survivor, "name").await.unwrap(),
        Value::str("Shared")
    );
    assert_eq!(
        records
            .search(&mut survivor, &Domain::TRUE)
            .await
            .unwrap()
            .len(),
        2
    );
    survivor.rollback().await.unwrap();
    drop(survivor);
    drop(other);
    h.cleanup().await.unwrap();
}
