//! Cache-only virtual references must not become persistence authority.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, RawVals};
use erp_test_models::base_users;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_virtual_compute");
static ISOLATED_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

#[model(name = "test_virtual.node", addon = "test_virtual_compute")]
pub struct Node {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub requested: Option<Arc<str>>,
    #[field(many2one, comodel = "test_virtual.node")]
    pub parent_id: Option<RecId>,
    #[field(one2many, comodel = "test_virtual.node", inverse_name = "parent_id")]
    pub child_ids: Vec<RecId>,
    #[field(
        many2one,
        comodel = "test_virtual.node",
        compute = "_resolve",
        store = true,
        precompute = false
    )]
    pub resolved_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "test_virtual.node",
        related = "resolved_id.parent_id"
    )]
    pub related_parent_id: Option<RecId>,
}
#[methods(Node, addon = "test_virtual_compute")]
impl Node {
    #[onchange("name")]
    async fn _capture_isolated(
        this: &RecordSet<Node>,
        tx: &mut Transaction,
    ) -> Result<erp_orm::OnchangeResult> {
        let name = this.get(tx, Node::NAME).await?;
        if matches!(
            name.as_deref(),
            Some("capture-isolated-id" | "capture-isolated-error")
        ) {
            ISOLATED_ID.store(this.rec_ids()[0].get(), std::sync::atomic::Ordering::SeqCst);
            if name.as_deref() == Some("capture-isolated-error") {
                this.set(tx, Node::REQUESTED, i64::MIN.to_string()).await?;
                let error = this.get(tx, Node::RESOLVED_ID).await.unwrap_err();
                assert!(
                    tx.is_poisoned(),
                    "the isolated compute really poisoned its scope"
                );
                return Err(error);
            }
        }
        Ok(erp_orm::OnchangeResult::default())
    }
    #[compute(depends = ["requested"])]
    fn _resolve(batch: &mut Batch<'_, Node>) -> Result<()> {
        for i in batch.indices() {
            let raw = batch.get(i, Node::REQUESTED)?;
            let target = match raw.as_deref() {
                None => Some(batch.ids()[i]),
                Some("null") => None,
                Some(s) => Some(
                    RecId::from_raw(
                        s.parse()
                            .map_err(|_| Error::validation("fixture invalid integer"))?,
                    )
                    .ok_or_else(|| Error::validation("fixture zero id"))?,
                ),
            };
            batch.set(i, Node::RESOLVED_ID, target)?;
        }
        Ok(())
    }
}

#[model(name = "test_virtual.pre", addon = "test_virtual_compute")]
pub struct Pre {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_virtual.pre",
        compute = "_self",
        store = true,
        precompute = true
    )]
    pub self_id: Option<RecId>,
}
#[methods(Pre, addon = "test_virtual_compute")]
impl Pre {
    #[compute(depends = ["name"])]
    fn _self(batch: &mut Batch<'_, Pre>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, Pre::SELF_ID, Some(batch.ids()[i]))?;
        }
        Ok(())
    }
}

#[model(name = "test_virtual.async", addon = "test_virtual_compute")]
pub struct AsyncNode {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_virtual.node", compute = "_allocate")]
    pub target_id: Option<RecId>,
}
#[methods(AsyncNode, addon = "test_virtual_compute")]
impl AsyncNode {
    #[compute(depends = ["name"])]
    async fn _allocate(
        this: &RecordSet<AsyncNode>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AsyncNode>,
    ) -> Result<()> {
        let nodes = tx.env().model::<Node>()?;
        for i in 0..this.len() {
            let target = nodes
                .new_record(tx, nodes.vals().set(Node::NAME, "Allocated inside compute"))
                .await?;
            out.set(i, AsyncNode::TARGET_ID, Some(target.rec_ids()[0]))?;
        }
        Ok(())
    }
}
async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn cache_compute_keeps_self_and_another_active_virtual_target() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let nodes = tx.env().model::<Node>().unwrap();
    let root = nodes
        .new_record(&mut tx, nodes.vals().set(Node::NAME, "Root"))
        .await
        .unwrap();
    let id = root.rec_ids()[0];
    assert!(id.is_new());
    assert_eq!(
        root.get(&mut tx, Node::RESOLVED_ID).await.unwrap(),
        Some(id)
    );
    let child = nodes
        .new_record(
            &mut tx,
            nodes
                .vals()
                .set(Node::NAME, "Child")
                .set(Node::PARENT_ID, Some(id))
                .set(Node::REQUESTED, id.get().to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        child.get(&mut tx, Node::RESOLVED_ID).await.unwrap(),
        Some(id)
    );
    assert_eq!(
        root.get(&mut tx, Node::CHILD_IDS).await.unwrap(),
        vec![child.rec_ids()[0]]
    );
    root.set(
        &mut tx,
        Node::REQUESTED,
        child.rec_ids()[0].get().to_string(),
    )
    .await
    .unwrap();
    assert_eq!(
        root.get(&mut tx, Node::RELATED_PARENT_ID).await.unwrap(),
        Some(id)
    );
    tx.flush().await.unwrap();
    assert_eq!(
        nodes
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        0
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn async_compute_can_reference_a_record_allocated_during_its_call() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<AsyncNode>().unwrap();
    let new = model
        .new_record(&mut tx, model.vals().set(AsyncNode::NAME, "Source"))
        .await
        .unwrap();
    let target = new
        .get(&mut tx, AsyncNode::TARGET_ID)
        .await
        .unwrap()
        .unwrap();
    assert!(target.is_new());
    let targets = new
        .as_records()
        .related(&mut tx, "target_id")
        .await
        .unwrap();
    assert_eq!(targets.rec_ids(), &[target]);
    assert_eq!(
        targets.value(&mut tx, "name").await.unwrap(),
        erp_sdk::Value::str("Allocated inside compute")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn originless_input_references_require_active_membership_and_correct_model() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let nodes = tx.env().model::<Node>().unwrap();
    let unknown = RecId::new_unique(100).unwrap();
    let err = tx
        .new_record(
            nodes.as_records(),
            RawVals::new().set_cell("parent_id", Cell::Id(unknown.get())),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Wrong value for test_virtual.node.parent_id: {} is not a new record of this transaction",
            unknown.get()
        )
    );
    let other = tx.env().model::<AsyncNode>().unwrap();
    let alien = other
        .new_record(&mut tx, other.vals().set(AsyncNode::NAME, "Wrong model"))
        .await
        .unwrap();
    let result = tx
        .new_record(
            nodes.as_records(),
            RawVals::new().set_cell("parent_id", Cell::Id(alien.rec_ids()[0].get())),
            None,
            None,
        )
        .await;
    let err = result.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Wrong value for test_virtual.node.parent_id: {} is not a new record of this transaction",
            alien.rec_ids()[0].get()
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rolled_back_virtual_identity_is_not_admitted_by_the_allocation_counter() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let nodes = tx.env().model::<Node>().unwrap();
    let live = nodes
        .new_record(&mut tx, nodes.vals().set(Node::NAME, "Before savepoint"))
        .await
        .unwrap();
    let mut stale = None;
    let err = tx
        .savepoint(async |tx: &mut Transaction| -> Result<()> {
            let new = nodes
                .new_record(tx, nodes.vals().set(Node::NAME, "Rolled back"))
                .await?;
            stale = Some(new.rec_ids()[0]);
            Err(Error::validation("fixture rollback"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "fixture rollback");
    let stale = stale.unwrap();
    let retained = nodes
        .new_record(
            &mut tx,
            nodes.vals().set(Node::PARENT_ID, Some(live.rec_ids()[0])),
        )
        .await
        .unwrap();
    assert_eq!(
        retained.get(&mut tx, Node::PARENT_ID).await.unwrap(),
        Some(live.rec_ids()[0])
    );
    let err = tx
        .new_record(
            nodes.as_records(),
            RawVals::new().set_cell("parent_id", Cell::Id(stale.get())),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Wrong value for test_virtual.node.parent_id: {} is not a new record of this transaction",
            stale.get()
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn persistence_and_preinsert_compute_do_not_accept_virtual_targets() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let nodes = tx.env().model::<Node>().unwrap();
    let virtual_target = nodes
        .new_record(&mut tx, nodes.vals().set(Node::NAME, "Unsaved"))
        .await
        .unwrap();
    let persisted = nodes
        .create(
            &mut tx,
            vec![nodes.vals().set(Node::NAME, "Persisted").set(
                Node::REQUESTED,
                virtual_target.rec_ids()[0].get().to_string(),
            )],
        )
        .await
        .unwrap();
    let err = persisted.get(&mut tx, Node::RESOLVED_ID).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Invalid value for test_virtual.node.resolved_id: {}",
            virtual_target.rec_ids()[0].get()
        )
    );
    assert!(tx.is_poisoned());
    let refused_commit = tx.commit().await.unwrap_err();
    assert_eq!(refused_commit.kind(), ErrorKind::Validation);
    assert_eq!(refused_commit.message(), err.message());
    tx.rollback().await.unwrap();
    let pre = tx.env().model::<Pre>().unwrap();
    let err = pre
        .create(&mut tx, vec![pre.vals().set(Pre::NAME, "Synthetic")])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    // The synthetic allocation counter is internal; assert the full stable message and
    // validate its varying ID rather than pinning an incidental allocation position.
    let (prefix, raw) = err.message().rsplit_once(": ").unwrap();
    assert_eq!(prefix, "Invalid value for test_virtual.pre.self_id");
    assert!(
        RecId::from_raw(raw.parse().unwrap())
            .unwrap()
            .unique_index()
            .is_some()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn failed_new_record_does_not_leave_an_admissible_identity() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let nodes = tx.env().model::<Node>().unwrap();
    let live = nodes
        .new_record(&mut tx, nodes.vals().set(Node::NAME, "Live"))
        .await
        .unwrap();
    let failed_id = RecId::new_unique(live.rec_ids()[0].unique_index().unwrap() + 1).unwrap();
    let vals = RawVals::new().set_commands(
        "child_ids",
        vec![erp_sdk::Command::Create(
            RawVals::new().set_cell("not_a_field", Cell::Bool(true)),
        )],
    );
    let err = tx
        .new_record(nodes.as_records(), vals, None, None)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Invalid field 'not_a_field' on model 'test_virtual.node'"
    );
    let err = tx
        .new_record(
            nodes.as_records(),
            RawVals::new().set_cell("parent_id", Cell::Id(failed_id.get())),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Wrong value for test_virtual.node.parent_id: {} is not a new record of this transaction",
            failed_id.get()
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn null_real_and_origin_backed_compute_targets_remain_supported() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let nodes = tx.env().model::<Node>().unwrap();
    let real = nodes
        .create(&mut tx, vec![nodes.vals().set(Node::NAME, "Real")])
        .await
        .unwrap();
    assert_eq!(
        real.get(&mut tx, Node::RESOLVED_ID).await.unwrap(),
        Some(real.rec_ids()[0])
    );
    let from_origin = tx
        .new_record(
            nodes.as_records(),
            RawVals::new(),
            Some(real.rec_ids()[0]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        from_origin.cell(&mut tx, "resolved_id").await.unwrap(),
        Cell::Id(from_origin.rec_ids()[0].get())
    );
    let null = nodes
        .new_record(&mut tx, nodes.vals().set(Node::REQUESTED, "null"))
        .await
        .unwrap();
    assert_eq!(null.get(&mut tx, Node::RESOLVED_ID).await.unwrap(), None);
    let points_real = nodes
        .new_record(
            &mut tx,
            nodes
                .vals()
                .set(Node::REQUESTED, real.rec_ids()[0].get().to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        points_real.get(&mut tx, Node::RESOLVED_ID).await.unwrap(),
        Some(real.rec_ids()[0])
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[model(name = "test_virtual.group", addon = "test_virtual_compute")]
pub struct Group {
    #[field(char)]
    pub requested: Option<Arc<str>>,
    #[field(char, compute = "_group")]
    pub a_label: Option<Arc<str>>,
    #[field(many2one, comodel = "test_virtual.node", compute = "_group")]
    pub z_target_id: Option<RecId>,
}
#[methods(Group, addon = "test_virtual_compute")]
impl Group {
    #[compute(depends = ["requested"])]
    fn _group(batch: &mut Batch<'_, Group>) -> Result<()> {
        for i in batch.indices() {
            let raw = batch
                .get(i, Group::REQUESTED)?
                .ok_or_else(|| Error::validation("fixture missing target"))?;
            let raw = raw
                .parse::<i64>()
                .map_err(|_| Error::validation("fixture invalid target"))?;
            let target =
                RecId::from_raw(raw).ok_or_else(|| Error::validation("fixture zero target"))?;
            batch.set(i, Group::A_LABEL, "Must publish atomically")?;
            batch.set(i, Group::Z_TARGET_ID, Some(target))?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn invalid_group_target_does_not_publish_an_earlier_scalar_output() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let groups = tx.env().model::<Group>().unwrap();
    let invalid = RecId::new_unique(1000).unwrap();
    let new = groups
        .new_record(
            &mut tx,
            groups
                .vals()
                .set(Group::REQUESTED, invalid.get().to_string()),
        )
        .await
        .unwrap();
    assert!(!tx.is_cached(new.as_records(), "a_label").unwrap());
    let err = new.get(&mut tx, Group::A_LABEL).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Wrong value for test_virtual.group.z_target_id: {} is not a new record of this transaction",
            invalid.get()
        )
    );
    assert!(!tx.is_cached(new.as_records(), "a_label").unwrap());
    assert!(!tx.is_cached(new.as_records(), "z_target_id").unwrap());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn mixed_real_and_new_destinations_cannot_share_virtual_reference_authority() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let nodes = tx.env().model::<Node>().unwrap();
    let target = nodes
        .new_record(&mut tx, nodes.vals().set(Node::NAME, "Cache target"))
        .await
        .unwrap();
    let raw = target.rec_ids()[0].get();
    let groups = tx.env().model::<Group>().unwrap();
    let real = groups
        .create(
            &mut tx,
            vec![groups.vals().set(Group::REQUESTED, raw.to_string())],
        )
        .await
        .unwrap();
    let new = groups
        .new_record(
            &mut tx,
            groups.vals().set(Group::REQUESTED, raw.to_string()),
        )
        .await
        .unwrap();
    let both = groups
        .as_records()
        .browse_ids([new.rec_ids()[0], real.rec_ids()[0]]);
    let err = both.fetch(&mut tx, Some(&["a_label"])).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!("Invalid value for test_virtual.group.z_target_id: {raw}")
    );
    assert!(!tx.is_cached(new.as_records(), "a_label").unwrap());
    assert!(!tx.is_cached(real.as_records(), "a_label").unwrap());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn isolated_onchange_revokes_its_identity_and_preserves_outer_membership() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let nodes = tx.env().model::<Node>().unwrap();
    let outer = nodes
        .new_record(&mut tx, nodes.vals().set(Node::NAME, "Outer"))
        .await
        .unwrap();
    let mut values = erp_sdk::ValueMap::new();
    values.insert("name".into(), Value::str("capture-isolated-id"));
    let spec = erp_orm::web::Spec::new().with("name", erp_orm::web::SpecItem::default());
    nodes
        .as_records()
        .onchange(&mut tx, values, vec!["name".into()], spec)
        .await
        .unwrap();
    let stale = ISOLATED_ID.load(std::sync::atomic::Ordering::SeqCst);
    assert!(RecId::from_raw(stale).unwrap().unique_index().is_some());
    assert_ne!(stale, outer.rec_ids()[0].get());
    let err = tx
        .new_record(
            nodes.as_records(),
            RawVals::new().set_cell("parent_id", Cell::Id(stale)),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Wrong value for test_virtual.node.parent_id: {stale} is not a new record of this transaction"
        )
    );
    let mut values = erp_sdk::ValueMap::new();
    values.insert("name".into(), Value::str("capture-isolated-error"));
    let spec = erp_orm::web::Spec::new().with("name", erp_orm::web::SpecItem::default());
    let err = nodes
        .as_records()
        .onchange(&mut tx, values, vec!["name".into()], spec)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Invalid value for test_virtual.node.resolved_id: {}",
            i64::MIN
        )
    );
    assert!(!tx.is_poisoned());
    let failed = ISOLATED_ID.load(std::sync::atomic::Ordering::SeqCst);
    let err = tx
        .new_record(
            nodes.as_records(),
            RawVals::new().set_cell("parent_id", Cell::Id(failed)),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Wrong value for test_virtual.node.parent_id: {failed} is not a new record of this transaction"
        )
    );
    tx.invalidate_all().await.unwrap();
    let child = nodes
        .new_record(
            &mut tx,
            nodes.vals().set(Node::PARENT_ID, Some(outer.rec_ids()[0])),
        )
        .await
        .unwrap();
    assert_eq!(
        child.get(&mut tx, Node::PARENT_ID).await.unwrap(),
        Some(outer.rec_ids()[0])
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn malformed_and_wrong_model_compute_targets_fail_with_exact_diagnostics() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let nodes = tx.env().model::<Node>().unwrap();
    let other = tx.env().model::<AsyncNode>().unwrap();
    let alien = other
        .new_record(&mut tx, other.vals().set(AsyncNode::NAME, "Alien"))
        .await
        .unwrap();
    let raw = alien.rec_ids()[0].get();
    let bad = nodes
        .new_record(&mut tx, nodes.vals().set(Node::REQUESTED, raw.to_string()))
        .await
        .unwrap();
    let err = bad.get(&mut tx, Node::RESOLVED_ID).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Wrong value for test_virtual.node.resolved_id: {raw} is not a new record of this transaction"
        )
    );
    tx.rollback().await.unwrap();
    let bad = nodes
        .new_record(
            &mut tx,
            nodes.vals().set(Node::REQUESTED, i64::MIN.to_string()),
        )
        .await
        .unwrap();
    let err = bad.get(&mut tx, Node::RESOLVED_ID).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Invalid value for test_virtual.node.resolved_id: {}",
            i64::MIN
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[model(name = "test_virtual.money_sync", addon = "test_virtual_compute")]
pub struct MoneySync {
    #[field(char)]
    pub requested: Option<Arc<str>>,
    #[field(char)]
    pub requested_shadow: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.currency",
        compute = "_currency",
        store = true
    )]
    pub currency_id: Option<RecId>,
    #[field(char, compute = "_currency", store = true)]
    pub probe: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_virtual.node",
        compute = "_currency",
        store = true
    )]
    pub shadow_id: Option<RecId>,
    #[field(monetary, currency_field = "currency_id")]
    pub amount: Decimal,
}
#[methods(MoneySync, addon = "test_virtual_compute")]
impl MoneySync {
    #[compute(depends = ["requested", "requested_shadow"])]
    fn _currency(batch: &mut Batch<'_, MoneySync>) -> Result<()> {
        for i in batch.indices() {
            let raw = batch
                .get(i, MoneySync::REQUESTED)?
                .ok_or_else(|| Error::validation("fixture missing currency"))?;
            let id = RecId::from_raw(
                raw.parse()
                    .map_err(|_| Error::validation("fixture invalid currency"))?,
            )
            .ok_or_else(|| Error::validation("fixture zero currency"))?;
            let shadow = parse_shadow(batch.get(i, MoneySync::REQUESTED_SHADOW)?)?;
            batch.set(i, MoneySync::CURRENCY_ID, Some(id))?;
            batch.set(i, MoneySync::SHADOW_ID, shadow)?;
            batch.set(
                i,
                MoneySync::PROBE,
                format!(
                    "currency-{}-shadow-{}",
                    id.get(),
                    shadow.map_or(0, RecId::get)
                ),
            )?;
        }
        Ok(())
    }
}

#[model(name = "test_virtual.money_candidate", addon = "test_virtual_compute")]
pub struct MoneyCandidate {
    #[field(char)]
    pub requested: Option<Arc<str>>,
    #[field(char)]
    pub requested_shadow: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.currency",
        compute = "_currency",
        store = true
    )]
    pub currency_id: Option<RecId>,
    #[field(char, compute = "_currency", store = true)]
    pub probe: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_virtual.node",
        compute = "_currency",
        store = true
    )]
    pub shadow_id: Option<RecId>,
    #[field(monetary, currency_field = "currency_id")]
    pub amount: Decimal,
}
#[methods(MoneyCandidate, addon = "test_virtual_compute")]
impl MoneyCandidate {
    #[compute(candidate, depends = ["requested", "requested_shadow"])]
    async fn _currency(
        this: &RecordSet<MoneyCandidate>,
        tx: &mut ReadTx<'_>,
        out: &mut Assign<'_, MoneyCandidate>,
    ) -> Result<()> {
        let shadows = tx.mapped(this, MoneyCandidate::REQUESTED_SHADOW).await?;
        for (i, raw) in tx
            .mapped(this, MoneyCandidate::REQUESTED)
            .await?
            .into_iter()
            .enumerate()
        {
            let raw = raw.ok_or_else(|| Error::validation("fixture missing currency"))?;
            let id = RecId::from_raw(
                raw.parse()
                    .map_err(|_| Error::validation("fixture invalid currency"))?,
            )
            .ok_or_else(|| Error::validation("fixture zero currency"))?;
            let shadow = parse_shadow(shadows[i].clone())?;
            out.set(i, MoneyCandidate::CURRENCY_ID, Some(id))?;
            out.set(i, MoneyCandidate::SHADOW_ID, shadow)?;
            out.set(
                i,
                MoneyCandidate::PROBE,
                format!(
                    "currency-{}-shadow-{}",
                    id.get(),
                    shadow.map_or(0, RecId::get)
                ),
            )?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn monetary_projection_checks_virtual_targets_before_currency_lookup_or_insert() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let invalid = RecId::new_unique(1000).unwrap().get();
    for model in ["test_virtual.money_sync", "test_virtual.money_candidate"] {
        let records = tx.env().records(model).unwrap();
        let vals = RawVals::new()
            .set_cell("requested", Cell::Str(invalid.to_string().into()))
            .set_cell("amount", Cell::Decimal(Decimal::ONE));
        let err = tx
            .new_record(&records, vals.clone(), None, None)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!(
                "Wrong value for {model}.currency_id: {invalid} is not a new record of this transaction"
            )
        );
        // The same virtual value is refused in pre-INSERT staging, without currency lookup.
        let err = records.create(&mut tx, vec![vals]).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!("Invalid value for {model}.currency_id: {invalid}")
        );
        tx.rollback().await.unwrap();
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn nested_failure_revokes_new_range_and_preserves_existing_reference_admission() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let nodes = tx.env().records("test_virtual.node").unwrap();
    let live = tx
        .new_record(
            &nodes,
            RawVals::new().set_cell("name", Cell::Str("Keep".into())),
            None,
            Some("keep-ref"),
        )
        .await
        .unwrap();
    let first = live.rec_ids()[0].unique_index().unwrap();
    let bad_children = || {
        RawVals::new().set_commands(
            "child_ids",
            [
                erp_sdk::Command::Create(
                    RawVals::new().set_cell("name", Cell::Str("Valid nested child".into())),
                ),
                erp_sdk::Command::Create(RawVals::new().set_cell("not_a_field", Cell::Bool(true))),
            ],
        )
    };
    let err = tx
        .new_record(&nodes, bad_children(), None, Some("failed-ref"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Invalid field 'not_a_field' on model 'test_virtual.node'"
    );
    let survivor = tx
        .new_record(
            &nodes,
            RawVals::new().set_cell("parent_id", Cell::Id(live.rec_ids()[0].get())),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        survivor.cell(&mut tx, "parent_id").await.unwrap(),
        Cell::Id(live.rec_ids()[0].get())
    );
    // These identities were allocated by the failed outer and successful first nested CREATE.
    for index in [first + 1, first + 2] {
        let raw = RecId::new_unique(index).unwrap().get();
        let err = tx
            .new_record(
                &nodes,
                RawVals::new().set_cell("parent_id", Cell::Id(raw)),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!(
                "Wrong value for test_virtual.node.parent_id: {raw} is not a new record of this transaction"
            )
        );
    }
    let retried = tx
        .new_record(&nodes, RawVals::new(), None, Some("failed-ref"))
        .await
        .unwrap();
    assert!(retried.rec_ids()[0].unique_index().unwrap() > first + 2);
    let err = tx
        .new_record(&nodes, bad_children(), None, Some("keep-ref"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Invalid field 'not_a_field' on model 'test_virtual.node'"
    );
    let survivor = tx
        .new_record(
            &nodes,
            RawVals::new().set_cell("parent_id", Cell::Id(live.rec_ids()[0].get())),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        survivor.cell(&mut tx, "parent_id").await.unwrap(),
        Cell::Id(live.rec_ids()[0].get())
    );
    let same = tx
        .new_record(&nodes, RawVals::new(), None, Some("keep-ref"))
        .await
        .unwrap();
    assert_eq!(same.rec_ids(), live.rec_ids());
    let child = tx
        .new_record(
            &nodes,
            RawVals::new().set_cell("parent_id", Cell::Id(live.rec_ids()[0].get())),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        child.cell(&mut tx, "parent_id").await.unwrap(),
        Cell::Id(live.rec_ids()[0].get())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn monetary_persisted_projection_refuses_an_active_virtual_currency() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for model in ["test_virtual.money_sync", "test_virtual.money_candidate"] {
        let currencies = tx.env().records("res.currency").unwrap();
        let real = currencies
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("VRT".into()))
                        .set_cell("symbol", Cell::Str("V".into())),
                ],
            )
            .await
            .unwrap();
        let money = tx.env().records(model).unwrap();
        let saved = money
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("requested", Cell::Str(real.ids()[0].to_string().into()))
                        .set_cell("amount", Cell::Decimal(Decimal::ONE)),
                ],
            )
            .await
            .unwrap();
        tx.flush().await.unwrap();
        let before = saved.cell(&mut tx, "amount").await.unwrap();
        let before_probe = saved.cell(&mut tx, "probe").await.unwrap();
        let virtual_currency = tx
            .new_record(
                &currencies,
                RawVals::new().set_cell("name", Cell::Str("DRF".into())),
                None,
                None,
            )
            .await
            .unwrap();
        let raw = virtual_currency.rec_ids()[0].get();
        let vals = RawVals::new()
            .set_cell("requested", Cell::Str(raw.to_string().into()))
            .set_cell("amount", Cell::Decimal(Decimal::ONE + Decimal::ONE));
        tx.invalidate_records(&saved, Some(&["probe"]))
            .await
            .unwrap();
        let err = tx
            .savepoint(async |tx: &mut Transaction| {
                assert!(!tx.is_cached(&saved, "probe")?);
                let error = saved.write(tx, vals).await.unwrap_err();
                // Admission may prefetch the old database sibling. Inspect its value;
                // cache presence alone cannot distinguish prefetch from publication.
                assert_eq!(saved.cell(tx, "probe").await.unwrap(), before_probe);
                Err::<(), _>(error)
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!("Invalid value for {model}.currency_id: {raw}")
        );
        assert_eq!(saved.cell(&mut tx, "amount").await.unwrap(), before);
        assert_eq!(saved.cell(&mut tx, "probe").await.unwrap(), before_probe);
        // Active cache membership also cannot authorize a synthetic INSERT destination.
        let vals = RawVals::new()
            .set_cell("requested", Cell::Str(raw.to_string().into()))
            .set_cell("amount", Cell::Decimal(Decimal::ONE));
        let err = money.create(&mut tx, vec![vals]).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!("Invalid value for {model}.currency_id: {raw}")
        );
        tx.rollback().await.unwrap();
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

fn parse_shadow(value: Option<Arc<str>>) -> Result<Option<RecId>> {
    value
        .map(|raw| {
            let raw = raw
                .parse::<i64>()
                .map_err(|_| Error::validation("fixture invalid shadow"))?;
            RecId::from_raw(raw).ok_or_else(|| Error::validation("fixture zero shadow"))
        })
        .transpose()
}

#[tokio::test]
async fn monetary_projection_validates_siblings_before_consuming_a_valid_currency() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let invalid = RecId::new_unique(1000).unwrap().get();
    for model in ["test_virtual.money_sync", "test_virtual.money_candidate"] {
        let currencies = tx.env().records("res.currency").unwrap();
        let currency = currencies
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("SIB".into()))
                        .set_cell("symbol", Cell::Str("S".into())),
                ],
            )
            .await
            .unwrap();
        let records = tx.env().records(model).unwrap();
        let good = RawVals::new()
            .set_cell("requested", Cell::Str(currency.ids()[0].to_string().into()))
            .set_cell("amount", Cell::Decimal(Decimal::ONE));
        let vals = good
            .clone()
            .set_cell("requested_shadow", Cell::Str(invalid.to_string().into()));
        let err = tx
            .new_record(&records, vals.clone(), None, None)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!(
                "Wrong value for {model}.shadow_id: {invalid} is not a new record of this transaction"
            )
        );
        let saved = records.create(&mut tx, vec![good]).await.unwrap();
        tx.flush().await.unwrap();
        let before = saved.cell(&mut tx, "probe").await.unwrap();
        tx.invalidate_records(&saved, Some(&["probe"]))
            .await
            .unwrap();
        let err = tx
            .savepoint(async |tx: &mut Transaction| {
                assert!(!tx.is_cached(&saved, "probe")?);
                let error = saved.write(tx, vals.clone()).await.unwrap_err();
                assert_eq!(saved.cell(tx, "probe").await.unwrap(), before);
                Err::<(), _>(error)
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!("Invalid value for {model}.shadow_id: {invalid}")
        );
        assert_eq!(saved.cell(&mut tx, "probe").await.unwrap(), before);
        let err = records.create(&mut tx, vec![vals]).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!("Invalid value for {model}.shadow_id: {invalid}")
        );
        tx.rollback().await.unwrap();
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[model(name = "test_virtual.self_read", addon = "test_virtual_compute")]
pub struct SelfRead {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char, compute = "_value", store = true, readonly = false)]
    pub value: Option<Arc<str>>,
}
#[methods(SelfRead, addon = "test_virtual_compute")]
impl SelfRead {
    #[compute(depends = ["name"], reads = ["value"])]
    async fn _value(
        this: &RecordSet<SelfRead>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SelfRead>,
    ) -> Result<()> {
        for (i, old) in this
            .mapped(tx, SelfRead::VALUE)
            .await?
            .into_iter()
            .enumerate()
        {
            // The compute's protected old value is null for a fresh record.
            out.set(
                i,
                SelfRead::VALUE,
                old.or_else(|| Some(Arc::from("Default"))),
            )?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn stored_new_compute_can_read_its_protected_old_value_without_reentry() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().model::<SelfRead>().unwrap();
    let fresh = records
        .new_record(&mut tx, records.vals().set(SelfRead::NAME, "Fresh"))
        .await
        .unwrap();
    assert_eq!(
        fresh
            .get(&mut tx, SelfRead::VALUE)
            .await
            .unwrap()
            .as_deref(),
        Some("Default")
    );
    fresh
        .set(&mut tx, SelfRead::VALUE, "Explicit")
        .await
        .unwrap();
    fresh.set(&mut tx, SelfRead::NAME, "Changed").await.unwrap();
    assert_eq!(
        fresh
            .get(&mut tx, SelfRead::VALUE)
            .await
            .unwrap()
            .as_deref(),
        Some("Explicit")
    );
    tx.flush().await.unwrap();
    assert_eq!(
        records
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        0
    );
    let real = records
        .create(
            &mut tx,
            vec![
                records
                    .vals()
                    .set(SelfRead::NAME, "Origin row")
                    .set(SelfRead::VALUE, "Origin"),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let expected = real.get(&mut tx, SelfRead::VALUE).await.unwrap();
    assert_eq!(expected.as_deref(), Some("Origin"));
    let from_origin = tx
        .new_record(
            records.as_records(),
            RawVals::new(),
            Some(real.rec_ids()[0]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        from_origin.cell(&mut tx, "value").await.unwrap(),
        Cell::Str(expected.unwrap())
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
