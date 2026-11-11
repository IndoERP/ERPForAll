//! Generic native base-write admission contract for partner address synchronization.
//! These fixture tests claim no whole PARTNER acceptance case.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_orm::{BeginOptions, Operation, StaticPolicy};
use erp_sdk::prelude::*;
use erp_test_models::base_users;
use erp_testkit::{DomainPolicy, FieldPolicy, Harness, Otherwise};

erp_sdk::addon!(name = "test_native_base_write", depends = ["base"]);

const MODEL: &str = "test_native_base_write.item";
const DESCRIPTION: &str = "Native base-write item";

#[model(
    name = "test_native_base_write.item",
    addon = "test_native_base_write",
    description = "Native base-write item"
)]
pub struct Item {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub amount: i32,
    #[field(integer, groups = "test_native_base_write.group_writer")]
    pub restricted_amount: i32,
    #[field(integer, generated = "amount + 1", generated_storage = "stored")]
    pub generated_amount: i32,
    #[field(integer, compute = "_compute_doubled", store)]
    pub doubled: i32,
    #[field(many2one, comodel = "test_native_base_write.item")]
    pub parent_id: Option<RecId>,
    #[field(
        one2many,
        comodel = "test_native_base_write.item",
        inverse_name = "parent_id"
    )]
    pub child_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_native_base_write.item",
        relation = "test_native_base_write_peer_rel",
        column1 = "left_id",
        column2 = "right_id"
    )]
    pub peer_ids: Vec<RecId>,
}

#[derive(Default)]
struct OverrideCalls(usize);

#[methods(Item, addon = "test_native_base_write")]
impl Item {
    #[method(override)]
    async fn write(
        this: &RecordSet<Item>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        tx.extension::<OverrideCalls>().0 += 1;
        if this
            .env()
            .context()
            .get("refuse_override")
            .is_some_and(|value| value.is_truthy())
        {
            return Err(Error::access("Native base-write fixture override refused"));
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[compute(depends = ["amount"])]
    fn _compute_doubled(batch: &mut Batch<'_, Item>) -> Result<()> {
        for index in batch.indices() {
            batch.set(index, Item::DOUBLED, batch.get(index, Item::AMOUNT)? * 2)?;
        }
        Ok(())
    }

    #[constrains("amount")]
    async fn _nonnegative(this: &RecordSet<Item>, tx: &mut Transaction) -> Result<()> {
        for record in this.iter() {
            if record.get(tx, Item::AMOUNT).await? < 0 {
                return Err(Error::validation(
                    "Native base-write amount must be nonnegative",
                ));
            }
        }
        Ok(())
    }
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &ADDON])
        .build()
        .await
        .unwrap()
}

async fn create(tx: &mut Transaction, amount: i32) -> Records {
    tx.env()
        .records(MODEL)
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("amount", Cell::Int(amount))],
        )
        .await
        .unwrap()
}

async fn add_user(tx: &mut Transaction) {
    tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'base-writer', 'base-writer', true)").await.unwrap();
}

fn assert_error(error: &Error, kind: erp_sdk::ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind);
    assert_eq!(error.message(), message);
}

#[tokio::test]
async fn base_write_refuses_records_from_another_registry_before_mutation() {
    let source = harness().await;
    let target = harness().await;
    let mut source_tx = source.begin().await.unwrap();
    let record = create(&mut source_tx, 2).await;
    let mut target_tx = target.begin().await.unwrap();
    let before = target_tx.query_count();
    let error = record
        .write_base(
            &mut target_tx,
            RawVals::new().set_cell("amount", Cell::Int(9)),
        )
        .await
        .unwrap_err();
    assert_error(
        &error,
        erp_sdk::ErrorKind::Programming,
        &format!("{record} belongs to another registry than this transaction's"),
    );
    assert_eq!(target_tx.query_count(), before);
    assert!(!target_tx.is_poisoned());
    assert_eq!(
        record.value(&mut source_tx, "amount").await.unwrap(),
        Value::Int(2)
    );
    drop(target_tx);
    drop(source_tx);
    target.cleanup().await.unwrap();
    source.cleanup().await.unwrap();
}

#[tokio::test]
async fn base_write_bypasses_overrides_and_recomputes_relations() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, 1).await;
    let child = create(&mut tx, 2).await;
    let peer = create(&mut tx, 3).await;
    let guarded = parent.with_context([("refuse_override", Value::Bool(true))]);
    let error = guarded
        .write(&mut tx, RawVals::new().set_cell("amount", Cell::Int(4)))
        .await
        .unwrap_err();
    assert_error(
        &error,
        erp_sdk::ErrorKind::Access,
        "Native base-write fixture override refused",
    );
    assert_eq!(tx.extension::<OverrideCalls>().0, 1);
    guarded
        .write_base(&mut tx, RawVals::new().set_cell("amount", Cell::Int(7)))
        .await
        .unwrap();
    assert_eq!(
        tx.extension::<OverrideCalls>().0,
        1,
        "the item override was skipped"
    );
    let values = RawVals::new()
        .set_commands(
            "child_ids",
            [Command::link(RecId::from_real(child.ids()[0]).unwrap())],
        )
        .set_commands(
            "peer_ids",
            [Command::set([RecId::from_real(peer.ids()[0]).unwrap()])],
        );
    parent.write_base(&mut tx, values).await.unwrap();
    assert_eq!(
        parent.value(&mut tx, "amount").await.unwrap(),
        Value::Int(7)
    );
    assert_eq!(
        parent.value(&mut tx, "doubled").await.unwrap(),
        Value::Int(14)
    );
    assert_eq!(
        child.value(&mut tx, "parent_id").await.unwrap(),
        Value::Int(parent.ids()[0])
    );
    assert_eq!(
        parent.cells(&mut tx, "peer_ids").await.unwrap(),
        vec![Cell::Ids(
            vec![RecId::from_real(peer.ids()[0]).unwrap()].into()
        )]
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        parent.value(&mut tx, "generated_amount").await.unwrap(),
        Value::Int(8)
    );
    assert_eq!(
        parent.value(&mut tx, "doubled").await.unwrap(),
        Value::Int(14)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn base_write_refuses_generated_assignments_even_for_empty_records() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records(MODEL).unwrap();
    let error = model
        .write_base(
            &mut tx,
            RawVals::new().set_cell("generated_amount", Cell::Int(9)),
        )
        .await
        .unwrap_err();
    assert_error(
        &error,
        erp_sdk::ErrorKind::Validation,
        "Generated field 'test_native_base_write.item.generated_amount' is read-only",
    );
    assert_eq!(tx.extension::<OverrideCalls>().0, 0);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn base_write_refuses_read_only_transactions() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let id = create(&mut setup, 2).await.ids()[0];
    setup.commit().await.unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).read_only("native-base-read"))
        .await
        .unwrap();
    let record = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
    let error = record
        .write_base(&mut tx, RawVals::new().set_cell("amount", Cell::Int(4)))
        .await
        .unwrap_err();
    assert_error(
        &error,
        erp_sdk::ErrorKind::Programming,
        "'native-base-read' is declared effect = read but tried to write 'test_native_base_write.item'; declare effect = write (or wider).",
    );
    assert_eq!(
        record.value(&mut tx, "amount").await.unwrap(),
        Value::Int(2)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn base_write_preserves_model_authorization() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .policy(StaticPolicy::SuperuserOnly)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    add_user(&mut tx).await;
    let record = create(&mut tx, 2).await;
    let error = record
        .with_user(2)
        .write_base(&mut tx, RawVals::new().set_cell("amount", Cell::Int(4)))
        .await
        .unwrap_err();
    assert_error(
        &error,
        erp_sdk::ErrorKind::Access,
        &format!("You are not allowed to write '{DESCRIPTION}' ({MODEL}) records."),
    );
    assert_eq!(
        record.value(&mut tx, "amount").await.unwrap(),
        Value::Int(2)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn base_write_preserves_record_authorization() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            MODEL,
            Operation::Write,
            Domain::condition("amount", Operator::Gt, 10).unwrap(),
        )
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    add_user(&mut tx).await;
    let record = create(&mut tx, 2).await;
    let error = record
        .with_user(2)
        .write_base(&mut tx, RawVals::new().set_cell("amount", Cell::Int(20)))
        .await
        .unwrap_err();
    assert_error(
        &error,
        erp_sdk::ErrorKind::Access,
        &format!(
            "You are not allowed to modify '{DESCRIPTION}' ({MODEL}) records.\n\nRecords: {MODEL}({},), User: 2",
            record.ids()[0]
        ),
    );
    assert_eq!(
        record.value(&mut tx, "amount").await.unwrap(),
        Value::Int(2)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn base_write_preserves_field_authorization_even_for_equal_values() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .policy(FieldPolicy::new().enforce_groups())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    add_user(&mut tx).await;
    let record = create(&mut tx, 2).await;
    let error = record
        .with_user(2)
        .write_base(
            &mut tx,
            RawVals::new().set_cell("restricted_amount", Cell::Int(0)),
        )
        .await
        .unwrap_err();
    assert_error(
        &error,
        erp_sdk::ErrorKind::Access,
        &format!(
            "You do not have enough rights to access the field \"restricted_amount\" on {DESCRIPTION} ({MODEL}). Please contact your system administrator.\n\nOperation: write"
        ),
    );
    assert_eq!(
        record.value(&mut tx, "amount").await.unwrap(),
        Value::Int(2)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn base_write_constraints_poison_and_savepoint_recovery_restores_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let record = create(&mut tx, 2).await;
    tx.flush().await.unwrap();
    let error = tx
        .savepoint(async |tx| {
            record
                .write_base(tx, RawVals::new().set_cell("amount", Cell::Int(-1)))
                .await
        })
        .await
        .unwrap_err();
    assert_error(
        &error,
        erp_sdk::ErrorKind::Validation,
        "Native base-write amount must be nonnegative",
    );
    assert!(!tx.is_poisoned());
    assert_eq!(
        record.value(&mut tx, "amount").await.unwrap(),
        Value::Int(2)
    );
    record
        .write_base(&mut tx, RawVals::new().set_cell("amount", Cell::Int(5)))
        .await
        .unwrap();
    let error = record
        .write_base(&mut tx, RawVals::new().set_cell("amount", Cell::Int(-2)))
        .await
        .unwrap_err();
    assert_error(
        &error,
        erp_sdk::ErrorKind::Validation,
        "Native base-write amount must be nonnegative",
    );
    assert!(tx.is_poisoned());
    let commit_error = tx.commit().await.unwrap_err();
    assert_error(
        &commit_error,
        erp_sdk::ErrorKind::Validation,
        "Native base-write amount must be nonnegative",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn base_write_is_not_a_method_dispatch_entry() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records(MODEL).unwrap();
    assert!(
        h.orm()
            .dispatch()
            .implementations(model.model_id(), "write_base")
            .is_empty()
    );
    let error = erp_orm::call_method::<(RawVals,), ()>(
        &mut tx,
        &model,
        "write_base",
        (RawVals::new().set_cell("amount", Cell::Int(4)),),
    )
    .await
    .unwrap_err();
    assert_error(
        &error,
        erp_sdk::ErrorKind::Programming,
        "'test_native_base_write.item' object has no attribute 'write_base'",
    );
    let error = erp_orm::call_method_json(&mut tx, &model, "write_base", "[{\"amount\":4}]")
        .await
        .unwrap_err();
    assert_error(
        &error,
        erp_sdk::ErrorKind::Programming,
        "'test_native_base_write.item' object has no attribute 'write_base'",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
