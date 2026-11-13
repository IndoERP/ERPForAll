//! Native category acceptance; descendant name search remains.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "fixture assertions"
)]
use erp_sdk::{Cell, Domain, ErrorKind, Operator, RawVals, Records, Value};
use erp_test_models::base_users;
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap()
}
async fn tag(tx: &mut erp_sdk::Transaction, name: &str, parent: Option<i64>) -> Records {
    let mut vals = RawVals::new().set_cell("name", Cell::Str(name.into()));
    if let Some(parent) = parent {
        vals.insert("parent_id".into(), Cell::Id(parent));
    }
    tx.env()
        .records("res.partner.category")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
}
async fn tree(tx: &mut erp_sdk::Transaction) -> [Records; 3] {
    let root = tag(tx, "Customer", None).await;
    let vip = tag(tx, "VIP", Some(root.ids()[0])).await;
    let gold = tag(tx, "Gold", Some(vip.ids()[0])).await;
    [root, vip, gold]
}
#[tokio::test]
async fn category_fields_defaults_and_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let b = tag(&mut tx, "B", None).await;
    let a = tag(&mut tx, "A", None).await;
    let a2 = tag(&mut tx, "A", None).await;
    for t in [&b, &a, &a2] {
        assert_eq!(t.value(&mut tx, "active").await.unwrap(), Value::Bool(true));
        let color = t.value(&mut tx, "color").await.unwrap();
        assert!(
            matches!(color, Value::Int(1..=11)),
            "default color {color:?}"
        );
    }
    let cats = tx.env().records("res.partner.category").unwrap();
    let found = cats.search(&mut tx, &Domain::TRUE).await.unwrap();
    assert_eq!(found.ids(), vec![a.ids()[0], a2.ids()[0], b.ids()[0]]);
    let err = tx
        .savepoint(async |tx| cats.create(tx, vec![RawVals::new()]).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "The operation cannot be completed: Missing required field 'Name' (name) for model 'Partner Tag' (res.partner.category)"
    );
    let fields = cats
        .fields_get(&mut tx, Some(&["name", "parent_path"]), None)
        .await
        .unwrap();
    assert_eq!(fields["name"]["translate"], Value::Bool(true));
    drop(tx);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn category_display_names_follow_ancestor_changes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let [root, vip, gold] = tree(&mut tx).await;
    for (t, expected) in [
        (&root, "Customer"),
        (&vip, "Customer / VIP"),
        (&gold, "Customer / VIP / Gold"),
    ] {
        assert_eq!(
            t.value(&mut tx, "display_name").await.unwrap(),
            Value::str(expected)
        );
    }
    root.write(
        &mut tx,
        RawVals::new().set_cell("name", Cell::Str("客户".into())),
    )
    .await
    .unwrap();
    assert_eq!(
        gold.value(&mut tx, "display_name").await.unwrap(),
        Value::str("客户 / VIP / Gold")
    );
    vip.write(&mut tx, RawVals::new().set_cell("parent_id", Cell::Null))
        .await
        .unwrap();
    assert_eq!(
        gold.value(&mut tx, "display_name").await.unwrap(),
        Value::str("VIP / Gold")
    );
    vip.write(
        &mut tx,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    assert_eq!(
        gold.value(&mut tx, "display_name").await.unwrap(),
        Value::str("VIP / Gold")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn category_flat_name_search_partial() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let only = tag(&mut tx, "buggy_test", None).await;
    let cats = tx.env().records("res.partner.category").unwrap();
    assert_eq!(
        cats.name_search(&mut tx, "buggy_test").await.unwrap(),
        vec![(only.ids()[0], Some("buggy_test".into()))]
    );
    let other = tag(&mut tx, "Supplier", None).await;
    let d = Domain::condition("display_name", Operator::NotILike, "buggy_test").unwrap();
    assert_eq!(cats.search(&mut tx, &d).await.unwrap().ids(), other.ids());
    drop(tx);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn category_parent_store_refuses_cycles() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let [root, vip, gold] = tree(&mut tx).await;
    let expected = format!("{}/{}/{}/", root.ids()[0], vip.ids()[0], gold.ids()[0]);
    assert_eq!(
        gold.value(&mut tx, "parent_path").await.unwrap(),
        Value::str(&expected)
    );
    for (child, parent) in [(&root, &gold), (&vip, &vip)] {
        let err = tx
            .savepoint(async |tx| {
                child
                    .write(
                        tx,
                        RawVals::new().set_cell("parent_id", Cell::Id(parent.ids()[0])),
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError);
        assert_eq!(
            err.message(),
            "You are creating a loop in your 'Partner Tag' records. A record cannot be a child of itself or one of its own sub-items. Please select a different parent."
        );
        assert_eq!(
            gold.value(&mut tx, "parent_path").await.unwrap(),
            Value::str(&expected)
        );
    }
    gold.write(
        &mut tx,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    let batch = tx
        .env()
        .records("res.partner.category")
        .unwrap()
        .browse([root.ids()[0], vip.ids()[0]])
        .unwrap();
    let err = tx
        .savepoint(async |tx| {
            batch
                .write(
                    tx,
                    RawVals::new().set_cell("parent_id", Cell::Id(gold.ids()[0])),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "You are creating a loop in your 'Partner Tag' records. A record cannot be a child of itself or one of its own sub-items. Please select a different parent."
    );
    assert_eq!(
        gold.value(&mut tx, "parent_path").await.unwrap(),
        Value::str(&expected)
    );
    assert_eq!(
        root.value(&mut tx, "parent_id").await.unwrap(),
        Value::False
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn category_deletion_cascades_and_removes_partner_links() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let [root, vip, gold] = tree(&mut tx).await;
    let p = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create_values(
            &mut tx,
            &[[
                ("name".to_owned(), Value::str("Tagged")),
                (
                    "category_id".to_owned(),
                    Value::list([Value::Int(gold.ids()[0])]),
                ),
            ]
            .into_iter()
            .collect()],
        )
        .await
        .unwrap();
    assert_eq!(
        p.value(&mut tx, "category_id").await.unwrap(),
        Value::list([Value::Int(gold.ids()[0])])
    );
    vip.unlink(&mut tx).await.unwrap();
    assert!(gold.exists(&mut tx).await.unwrap().is_empty());
    assert!(!root.exists(&mut tx).await.unwrap().is_empty());
    assert_eq!(
        p.value(&mut tx, "category_id").await.unwrap(),
        Value::list([])
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        p.value(&mut tx, "category_id").await.unwrap(),
        Value::list([])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn category_color_default_preserves_explicit_and_context_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tx.env().records("res.partner.category").unwrap();
    assert!(
        cats.default_get(&mut tx, &["name"])
            .await
            .unwrap()
            .get("color")
            .is_none()
    );
    let context = cats.with_context([("default_color", Value::Int(7))]);
    assert_eq!(
        context
            .default_get(&mut tx, &["color"])
            .await
            .unwrap()
            .get("color"),
        Some(&Value::Int(7))
    );
    let explicit = context
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Explicit zero".into()))
                    .set_cell("color", Cell::Int(0)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        explicit.value(&mut tx, "color").await.unwrap(),
        Value::Int(0)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn category_label_translation_uses_reader_language() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for (name, code, url_code) in [("English", "en_US", "en"), ("French", "fr_FR", "fr")] {
        let existing = tx
            .query("SELECT id FROM res_lang WHERE code = $1", &[&code])
            .await
            .unwrap();
        if existing.is_empty() {
            tx.env()
                .records("res.lang")
                .unwrap()
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(name.into()))
                            .set_cell("code", Cell::Str(code.into()))
                            .set_cell("url_code", Cell::Str(url_code.into()))
                            .set_cell("direction", Cell::Str("ltr".into()))
                            .set_cell("active", Cell::Bool(true)),
                    ],
                )
                .await
                .unwrap();
        }
    }
    let [root, _vip, gold] = tree(&mut tx).await;
    let fr = root.with_context([("lang", Value::str("fr_FR"))]);
    fr.write(
        &mut tx,
        RawVals::new().set_cell("name", Cell::Str("Client".into())),
    )
    .await
    .unwrap();
    assert_eq!(
        gold.with_context([("lang", Value::str("fr_FR"))])
            .value(&mut tx, "display_name")
            .await
            .unwrap(),
        Value::str("Client / VIP / Gold")
    );
    assert_eq!(
        gold.with_context([("lang", Value::str("en_US"))])
            .value(&mut tx, "display_name")
            .await
            .unwrap(),
        Value::str("Customer / VIP / Gold")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn category_label_output_limit_is_checked_before_composition() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let exact = "a".repeat(1_048_576);
    let root = tag(&mut tx, &exact, None).await;
    assert_eq!(
        root.value(&mut tx, "display_name").await.unwrap(),
        Value::str(&exact)
    );
    let child = tag(&mut tx, "b", Some(root.ids()[0])).await;
    let err = child.value(&mut tx, "display_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Partner tag label output exceeds the 1048576-byte limit"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Run a case on a thread with the 2 MiB stack of a default test thread, so any per-level
/// recursion in label, invalidation or depth handling aborts the case instead of passing.
fn on_default_stack<F: std::future::Future<Output = ()> + 'static>(case: fn() -> F) {
    std::thread::Builder::new()
        .name("deep-tag-tree".into())
        .stack_size(2 << 20)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(case());
        })
        .unwrap()
        .join()
        .unwrap();
}

const DEPTH_LIMIT_MESSAGE: &str = "Partner tag hierarchy exceeds depth 256";

#[test]
fn category_depth_limit_admits_256_and_refuses_257() {
    on_default_stack(depth_limit_case);
}

async fn depth_limit_case() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut ids: Vec<i64> = Vec::new();
    for level in 0..256 {
        let parent = ids.last().copied();
        let created = tag(&mut tx, &format!("Depth {level}"), parent).await;
        ids.push(created.ids()[0]);
    }
    let cats = tx.env().records("res.partner.category").unwrap();
    let leaf = cats.browse([ids[255]]).unwrap();
    let expected = (0..256)
        .map(|level| format!("Depth {level}"))
        .collect::<Vec<_>>()
        .join(" / ");
    assert_eq!(
        leaf.value(&mut tx, "display_name").await.unwrap(),
        Value::str(&expected)
    );

    // Creating level 257 is refused with the typed limit error and creates nothing.
    let err = tx
        .savepoint(async |tx| {
            cats.create(
                tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Too deep".into()))
                        .set_cell("parent_id", Cell::Id(ids[255])),
                ],
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), DEPTH_LIMIT_MESSAGE);
    assert_eq!(
        cats.search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        256
    );

    // A move is measured with the moved subtree: a two-level subtree below depth 255 is refused,
    // its single leaf is admitted at exactly depth 256.
    let loose_root = tag(&mut tx, "Loose root", None).await;
    let loose_leaf = tag(&mut tx, "Loose leaf", Some(loose_root.ids()[0])).await;
    let err = tx
        .savepoint(async |tx| {
            loose_root
                .write(tx, RawVals::new().set_cell("parent_id", Cell::Id(ids[254])))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), DEPTH_LIMIT_MESSAGE);
    assert_eq!(
        loose_root.value(&mut tx, "parent_id").await.unwrap(),
        Value::False
    );
    loose_leaf
        .write(
            &mut tx,
            RawVals::new().set_cell("parent_id", Cell::Id(ids[254])),
        )
        .await
        .unwrap();
    assert_eq!(
        loose_leaf.value(&mut tx, "display_name").await.unwrap(),
        Value::str(format!(
            "{} / Loose leaf",
            (0..255)
                .map(|level| format!("Depth {level}"))
                .collect::<Vec<_>>()
                .join(" / ")
        ))
    );

    // Renaming the root refreshes the deepest label without nesting a compute per level.
    cats.browse([ids[0]])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("Renamed".into())),
        )
        .await
        .unwrap();
    let renamed = leaf.value(&mut tx, "display_name").await.unwrap();
    assert_eq!(
        renamed,
        Value::str(expected.replacen("Depth 0", "Renamed", 1))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[test]
fn category_label_refuses_a_chain_written_past_the_limit() {
    on_default_stack(deep_chain_read_case);
}

async fn deep_chain_read_case() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tx.env().records("res.partner.category").unwrap();
    let rows = cats
        .create(
            &mut tx,
            (0..257)
                .map(|i| RawVals::new().set_cell("name", Cell::Str(format!("Depth {i}").into())))
                .collect(),
        )
        .await
        .unwrap();
    let ids = rows.ids();
    tx.flush().await.unwrap();
    tx.execute("WITH ordered AS (SELECT id, lag(id) OVER (ORDER BY position) AS parent, array_to_string(array_agg(id) OVER (ORDER BY position), '/') || '/' AS path FROM unnest($1::int8[]) WITH ORDINALITY fixture(id, position)) UPDATE res_partner_category category SET parent_id=ordered.parent, parent_path=ordered.path FROM ordered WHERE category.id=ordered.id", &[&ids]).await.unwrap();
    tx.invalidate_all().await.unwrap();
    let leaf = cats.browse([*ids.last().unwrap()]).unwrap();
    let err = leaf.value(&mut tx, "display_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), DEPTH_LIMIT_MESSAGE);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Saving without a name is still refused.
#[tokio::test]
async fn new_tag_onchange_accepts_the_empty_required_translated_name() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tx.env().records("res.partner.category").unwrap();
    let values: erp_sdk::ValueMap = [
        ("color".to_owned(), Value::Int(3)),
        ("active".to_owned(), Value::Bool(true)),
    ]
    .into_iter()
    .collect();
    let spec = erp_orm::Spec::new()
        .field("name")
        .field("color")
        .field("active");
    let result = cats
        .onchange(&mut tx, values, Vec::new(), spec)
        .await
        .expect("first onchange of a new tag");
    let Value::Map(result) = result else {
        panic!("onchange result is not a map: {result:?}")
    };
    let Some(Value::Map(value)) = result.get("value") else {
        panic!("onchange result has no value map: {result:?}")
    };
    assert_eq!(value.get("name"), Some(&Value::False), "{value:?}");
    let err = tx
        .savepoint(async |tx| cats.create(tx, vec![RawVals::new()]).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn category_ancestor_hidden_by_record_rule_fails_with_access_error() {
    let allowed = Domain::condition("name", Operator::Ne, "Hidden").unwrap();
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule("res.partner.category", erp_sdk::Operation::Read, allowed)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'demo', 'demo', true, false)")
        .await
        .unwrap();
    let hidden = tag(&mut tx, "Hidden", None).await;
    let visible = tag(&mut tx, "Visible Child", Some(hidden.ids()[0])).await;
    let visible_id = visible.ids()[0];
    tx.commit().await.unwrap();

    let mut user_tx = h.begin_as(2).await.unwrap();
    let cats = user_tx.env().records("res.partner.category").unwrap();
    let child = cats.browse([visible_id]).unwrap();
    let err = child.value(&mut user_tx, "display_name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    drop(user_tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn category_display_name_draft_root() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tx.env().records("res.partner.category").unwrap();
    let values: erp_sdk::ValueMap = [
        ("name".to_owned(), Value::str("Draft Root")),
        ("color".to_owned(), Value::Int(1)),
        ("active".to_owned(), Value::Bool(true)),
    ]
    .into_iter()
    .collect();
    let spec = erp_orm::Spec::new().field("name").field("display_name");
    let result = cats
        .onchange(&mut tx, values, vec!["name".into()], spec)
        .await
        .expect("onchange on draft root category");
    let Value::Map(result) = result else {
        panic!("onchange result is not a map: {result:?}")
    };
    let Some(Value::Map(value)) = result.get("value") else {
        panic!("onchange result has no value map: {result:?}")
    };
    assert_eq!(value.get("display_name"), Some(&Value::str("Draft Root")));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn category_display_name_draft_child() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let [_root, vip, _gold] = tree(&mut tx).await;
    let cats = tx.env().records("res.partner.category").unwrap();
    let values: erp_sdk::ValueMap = [
        ("name".to_owned(), Value::str("Draft Child")),
        ("parent_id".to_owned(), Value::Int(vip.ids()[0])),
        ("active".to_owned(), Value::Bool(true)),
    ]
    .into_iter()
    .collect();
    let spec = erp_orm::Spec::new()
        .field("name")
        .field("parent_id")
        .field("display_name");
    let result = cats
        .onchange(
            &mut tx,
            values,
            vec!["parent_id".into(), "name".into()],
            spec,
        )
        .await
        .expect("onchange on draft child category");
    let Value::Map(result) = result else {
        panic!("onchange result is not a map: {result:?}")
    };
    let Some(Value::Map(value)) = result.get("value") else {
        panic!("onchange result has no value map: {result:?}")
    };
    assert_eq!(
        value.get("display_name"),
        Some(&Value::str("Customer / VIP / Draft Child"))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[test]
fn category_display_name_draft_depth_refusal() {
    on_default_stack(draft_depth_refusal_case);
}

async fn draft_depth_refusal_case() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut ids: Vec<i64> = Vec::new();
    for level in 0..256 {
        let parent = ids.last().copied();
        let created = tag(&mut tx, &format!("Depth {level}"), parent).await;
        ids.push(created.ids()[0]);
    }
    let cats = tx.env().records("res.partner.category").unwrap();
    let values: erp_sdk::ValueMap = [
        ("name".to_owned(), Value::str("Draft Past Limit")),
        ("parent_id".to_owned(), Value::Int(ids[255])),
        ("active".to_owned(), Value::Bool(true)),
    ]
    .into_iter()
    .collect();
    let spec = erp_orm::Spec::new()
        .field("name")
        .field("parent_id")
        .field("display_name");
    let err = cats
        .onchange(
            &mut tx,
            values,
            vec!["parent_id".into(), "name".into()],
            spec,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), DEPTH_LIMIT_MESSAGE);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn category_display_name_draft_parent_id_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tx.env().records("res.partner.category").unwrap();
    let virtual_parent_id = erp_fields::RecId::new_unique(1).unwrap();
    let values: erp_sdk::ValueMap = [
        ("name".to_owned(), Value::str("Draft Under Draft")),
        ("parent_id".to_owned(), Value::Int(virtual_parent_id.get())),
    ]
    .into_iter()
    .collect();
    let spec = erp_orm::Spec::new()
        .field("name")
        .field("parent_id")
        .field("display_name");
    let err = cats
        .onchange(
            &mut tx,
            values,
            vec!["parent_id".into(), "name".into()],
            spec,
        )
        .await
        .unwrap_err();
    // A many2one value is a database id: a client never names a draft record as a plain
    // parent value (drafts only exist inside x2many commands), so the value check refuses it
    // before the compute's chained-draft guard can run.
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Invalid value for res.partner.category.parent_id: {}",
            virtual_parent_id.get()
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
