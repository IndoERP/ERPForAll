//! S6: actual creates and transaction lifecycle, not consumer delivery.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::{Cell, Error, ErrorKind};
use erp_orm::{BeginOptions, OperationIdentity, RawVals, SUPERUSER_ID, Transaction};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_test_models::base_users;
use erp_testkit::Harness;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_capture",
    depends: &["base"],
    exact_only: false,
};
const EVENTFUL: &str = "test.capture";
const PLAIN: &str = "test.capture.plain";
const TABLE: &str = "\"erp_event\".\"ir_event_outbox\"";

async fn harness() -> Harness {
    let fields = const { &[FieldDecl::char("name"), FieldDecl::integer("number")] };
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new("test_capture", EVENTFUL)
                .eventful(true)
                .fields(fields),
            ModelDecl::new("test_capture", PLAIN).fields(fields),
        ])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.commit().await.unwrap();
    h
}

async fn begin(h: &Harness) -> Transaction {
    h.orm()
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
}

async fn create(tx: &mut Transaction, name: &str) -> Vec<i64> {
    tx.env()
        .records(EVENTFUL)
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str(name.into()))],
        )
        .await
        .unwrap()
        .ids()
}

async fn count(tx: &mut Transaction, table: &str) -> i64 {
    tx.query(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .unwrap()[0]
        .get(0)
}

fn inserts(tx: &Transaction) -> usize {
    tx.captured()
        .iter()
        .filter(|s| s.starts_with("INSERT INTO \"erp_event\".\"ir_event_outbox\""))
        .count()
}

#[tokio::test]
async fn actual_creates_commit_atomically_in_one_ordered_insert() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    let first = create(&mut tx, "private value one").await;
    let second = create(&mut tx, "private value two").await;
    assert_eq!(count(&mut tx, TABLE).await, 0);
    let mut observer = h.begin().await.unwrap();
    assert_eq!(count(&mut observer, "test_capture").await, 0);
    assert_eq!(count(&mut observer, TABLE).await, 0);
    observer.rollback().await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 1);
    let mut observer = h.begin().await.unwrap();
    assert_eq!(count(&mut observer, "test_capture").await, 2);
    let events = observer.query(&format!("SELECT record_ids::text, field_names, kind, model, tx_ordinal, operation_id::text, event_id::text FROM {TABLE} ORDER BY id"), &[]).await.unwrap();
    assert_eq!(events.len(), 2);
    for (row, ids) in events.iter().zip([first, second]) {
        let captured: Vec<i64> = serde_json::from_str(row.get::<_, &str>(0)).unwrap();
        assert_eq!(captured, ids);
        let fields: Vec<String> = row.get(1);
        assert!(fields.iter().any(|f| f == "name"));
        assert!(!fields.iter().any(|f| f.contains("private value")));
        assert_eq!(row.get::<_, String>(2), "create");
        assert_eq!(row.get::<_, String>(3), EVENTFUL);
        assert!(!row.get::<_, String>(6).is_empty());
    }
    assert_eq!(events[0].get::<_, i64>(4), 1);
    assert_eq!(events[1].get::<_, i64>(4), 2);
    assert_eq!(events[0].get::<_, String>(5), events[1].get::<_, String>(5));
    observer.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn empty_and_non_eventful_creates_send_no_outbox_statement() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.env()
        .records(EVENTFUL)
        .unwrap()
        .create(&mut tx, vec![])
        .await
        .unwrap();
    tx.env()
        .records(PLAIN)
        .unwrap()
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 0);
    let mut check = h.begin().await.unwrap();
    assert_eq!(count(&mut check, TABLE).await, 0);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn eventful_create_without_identity_fails_before_insert() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    let error = tx
        .env()
        .records(EVENTFUL)
        .unwrap()
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "outbox: eventful mutation requires an OperationIdentity"
    );
    assert!(!tx.captured().iter().any(|s| s.starts_with("INSERT")));
    assert_eq!(count(&mut tx, "test_capture").await, 0);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn released_create_survives_sibling_rollback_but_root_rollback_discards() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let root = create(&mut tx, "root").await;
    let released = tx
        .savepoint(async |tx| Ok(create(tx, "released").await))
        .await
        .unwrap();
    let error = tx
        .savepoint(async |tx| {
            create(tx, "discarded").await;
            Err::<(), Error>(Error::programming("intentional rollback"))
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), "intentional rollback");
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let events = check
        .query(
            &format!("SELECT record_ids::text FROM {TABLE} ORDER BY id"),
            &[],
        )
        .await
        .unwrap();
    let actual: Vec<Vec<i64>> = events
        .iter()
        .map(|r| serde_json::from_str(r.get::<_, &str>(0)).unwrap())
        .collect();
    assert_eq!(actual, vec![root, released]);
    check.rollback().await.unwrap();
    create(&mut tx, "root rollback").await;
    tx.rollback().await.unwrap();
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert_eq!(count(&mut check, TABLE).await, 2);
    assert_eq!(count(&mut check, "test_capture").await, 2);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn failed_capture_insert_rolls_back_business_change() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    setup
        .execute(&format!("DROP TABLE {TABLE}"), &[])
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let mut tx = begin(&h).await;
    create(&mut tx, "must roll back").await;
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("42P01"));
    // PostgreSQL's quoted relation diagnostic is stable; wrapper context may be added by ORM.
    assert!(error.message().contains("ir_event_outbox"), "{error}");
    let mut check = h.begin().await.unwrap();
    assert_eq!(count(&mut check, "test_capture").await, 0);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn pipelined_commit_persists_create_and_failed_last_statement_discards_it() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    create(&mut tx, "committed").await;
    tx.commit_pipelined(None).await.unwrap();
    let mut tx = begin(&h).await;
    create(&mut tx, "last failed").await;
    let params = [];
    let last = erp_orm::FinalStatement::new("SELECT 1/0", &params);
    let error = tx.commit_pipelined(Some(last)).await.unwrap_err();
    match error {
        erp_orm::CommitError::RolledBack(error) => {
            assert_eq!(error.kind(), ErrorKind::Database);
            assert_eq!(error.message(), "division by zero");
        }
        other => panic!("expected rollback, got {other:?}"),
    }
    let mut check = h.begin().await.unwrap();
    assert_eq!(count(&mut check, TABLE).await, 1);
    assert_eq!(count(&mut check, "test_capture").await, 1);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn large_create_keeps_every_actual_id_without_invented_ranges() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    let records = tx
        .env()
        .records(EVENTFUL)
        .unwrap()
        .create(
            &mut tx,
            (0..257)
                .map(|n| RawVals::new().set_cell("number", Cell::Int(n)))
                .collect(),
        )
        .await
        .unwrap();
    let expected = records.ids();
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 1);
    let mut check = h.begin().await.unwrap();
    let rows = check
        .query(
            &format!("SELECT record_ids::text FROM {TABLE} ORDER BY id"),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let ids: Vec<i64> = rows
        .iter()
        .flat_map(|r| {
            let chunk: Vec<i64> = serde_json::from_str(r.get::<_, &str>(0)).unwrap();
            assert!(chunk.len() <= 256);
            chunk
        })
        .collect();
    assert_eq!(ids, expected);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn eventful_upsert_retains_business_key_validation() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let ids = create(&mut tx, "original").await;
    tx.commit().await.unwrap();
    let records = tx.env().records(EVENTFUL).unwrap().browse(ids).unwrap();
    let error = records
        .upsert(&mut tx, erp_orm::UpsertCall::new("name").with_rows([]))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "upsert on 'test.capture': no business key 'name' (declared keys: none)"
    );
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert_eq!(count(&mut check, TABLE).await, 1);
    assert_eq!(count(&mut check, "test_capture").await, 1);
    assert_eq!(
        check
            .query("SELECT name FROM test_capture", &[])
            .await
            .unwrap()[0]
            .get::<_, String>(0),
        "original"
    );
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cancelled_finalization_cannot_drop_events_and_then_commit_business_rows() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    create(&mut tx, "cancelled commit").await;
    let pid: i32 = tx.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    let mut blocker = h.begin().await.unwrap();
    blocker
        .execute(&format!("LOCK TABLE {TABLE} IN ACCESS EXCLUSIVE MODE"), &[])
        .await
        .unwrap();
    let mut observer = h.begin().await.unwrap();
    let mut committing = Box::pin(tx.commit());
    // This barrier observes the actual INSERT waiting for our table lock. The timeout is
    // admission for a hung fixture, not a timing assertion about the product.
    let mut waiting = Box::pin(tokio::time::timeout(
        std::time::Duration::from_secs(60),
        async {
            loop {
                let rows = observer.query(
                "SELECT wait_event_type = 'Lock' AND query LIKE '%ir_event_outbox%' FROM pg_stat_activity WHERE pid = $1",
                &[&pid],
            ).await.unwrap();
                if rows[0].get::<_, Option<bool>>(0) == Some(true) {
                    break;
                }
                // pg_stat_activity snapshots are transaction-local; start a new observation.
                observer.rollback().await.unwrap();
                tokio::task::yield_now().await;
            }
        },
    ));
    tokio::select! {
        outcome = &mut committing => panic!("commit passed the held lock: {outcome:?}"),
        outcome = &mut waiting => outcome.expect("final INSERT never reached the lock barrier"),
    }
    drop(waiting);
    drop(committing);
    blocker.rollback().await.unwrap();
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), "outbox: capture is frozen for commit");
    // End the observer's old snapshot before checking the rollback.
    observer.rollback().await.unwrap();
    assert_eq!(count(&mut observer, TABLE).await, 0);
    assert_eq!(count(&mut observer, "test_capture").await, 0);
    observer.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn non_eventful_unlink_cannot_indirectly_change_eventful_rows() {
    use erp_fields::OnDelete;
    for transitive in [false, true] {
        for action in [OnDelete::Cascade, OnDelete::SetNull] {
            let target = if transitive {
                "test.capture.bridge"
            } else {
                PLAIN
            };
            let fields = Box::leak(
                vec![
                    FieldDecl::char("name"),
                    FieldDecl::many2one("root_id", target).ondelete(action),
                ]
                .into_boxed_slice(),
            );
            let h =
                Harness::builder([&base_users::ADDON, &ADDON])
                    .with_decls([
                        ModelDecl::new("test_capture", PLAIN)
                            .fields(const { &[FieldDecl::char("name")] }),
                        ModelDecl::new("test_capture", "test.capture.bridge").fields(
                            const {
                                &[
                                    FieldDecl::many2one("root_id", PLAIN)
                                        .ondelete(OnDelete::Cascade),
                                ]
                            },
                        ),
                        ModelDecl::new("test_capture", EVENTFUL)
                            .eventful(true)
                            .fields(fields),
                    ])
                    .build()
                    .await
                    .unwrap();
            let mut tx = begin(&h).await;
            tx.install_event_schema().await.unwrap();
            let roots = tx
                .env()
                .records(PLAIN)
                .unwrap()
                .create(&mut tx, vec![RawVals::new()])
                .await
                .unwrap();
            let mut parent = roots.ids()[0];
            if transitive {
                parent = tx
                    .env()
                    .records("test.capture.bridge")
                    .unwrap()
                    .create(
                        &mut tx,
                        vec![RawVals::new().set_cell("root_id", Cell::Id(parent))],
                    )
                    .await
                    .unwrap()
                    .ids()[0];
            }
            tx.env()
                .records(EVENTFUL)
                .unwrap()
                .create(
                    &mut tx,
                    vec![RawVals::new().set_cell("root_id", Cell::Id(parent))],
                )
                .await
                .unwrap();
            tx.commit().await.unwrap();
            // New transaction and empty record cache; no compute dependents exist in this fixture.
            let mut tx = begin(&h).await;
            let roots = tx
                .env()
                .records(PLAIN)
                .unwrap()
                .browse(roots.ids())
                .unwrap();
            let error = roots.unlink(&mut tx).await.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(
                error.message(),
                "outbox: eventful unlink capture for model 'test.capture' is not implemented until S7-S9"
            );
            tx.commit().await.unwrap();
            let mut check = h.begin().await.unwrap();
            assert_eq!(count(&mut check, "test_capture_plain").await, 1);
            assert_eq!(count(&mut check, "test_capture").await, 1);
            assert_eq!(count(&mut check, TABLE).await, 1);
            assert_eq!(
                check
                    .query("SELECT root_id FROM test_capture", &[])
                    .await
                    .unwrap()[0]
                    .get::<_, i64>(0),
                parent
            );
            check.rollback().await.unwrap();
            h.cleanup().await.unwrap();
        }
    }
}

fn default_lines(_: &erp_registry::DefaultContext) -> erp_fields::Value {
    relation_default_value()
}

fn relation_default_value() -> erp_fields::Value {
    erp_fields::Value::list([erp_fields::Value::Map(std::sync::Arc::new(
        [("name".to_owned(), erp_fields::Value::str("default child"))]
            .into_iter()
            .collect(),
    ))])
}

#[tokio::test]
async fn resolved_relation_defaults_cannot_bypass_create_preflight() {
    for field_default in [false, true] {
        let mut lines = FieldDecl::one2many("lines", PLAIN, "owner_id");
        if field_default {
            lines = lines.default_fn(default_lines);
        }
        let fields = Box::leak(vec![FieldDecl::char("name"), lines].into_boxed_slice());
        let h = Harness::builder([&base_users::ADDON, &ADDON])
            .with_decls([
                ModelDecl::new("test_capture", EVENTFUL)
                    .eventful(true)
                    .fields(fields),
                ModelDecl::new("test_capture", PLAIN).fields(
                    const {
                        &[
                            FieldDecl::char("name"),
                            FieldDecl::many2one("owner_id", EVENTFUL),
                        ]
                    },
                ),
            ])
            .build()
            .await
            .unwrap();
        let mut tx = begin(&h).await;
        tx.install_event_schema().await.unwrap();
        tx.commit().await.unwrap();
        tx.capture_statements(true);
        let mut records = tx.env().records(EVENTFUL).unwrap();
        if !field_default {
            records = records.with_context([("default_lines", relation_default_value())]);
        }
        let error = records
            .create(&mut tx, vec![RawVals::new()])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "outbox: eventful relation capture for model 'test.capture' is not implemented until S7-S9"
        );
        assert!(!tx.captured().iter().any(|s| s.starts_with("INSERT")));
        tx.rollback().await.unwrap();
        let mut check = h.begin().await.unwrap();
        assert_eq!(count(&mut check, "test_capture").await, 0);
        assert_eq!(count(&mut check, "test_capture_plain").await, 0);
        assert_eq!(count(&mut check, TABLE).await, 0);
        check.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn cancelled_business_insert_cannot_commit_without_its_capture() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let pid: i32 = tx.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    let mut blocker = h.begin().await.unwrap();
    blocker
        .execute("LOCK TABLE test_capture IN SHARE MODE", &[])
        .await
        .unwrap();
    let mut observer = h.begin().await.unwrap();
    let records = tx.env().records(EVENTFUL).unwrap();
    let mut creating = Box::pin(records.create(&mut tx, vec![RawVals::new()]));
    let mut waiting = Box::pin(tokio::time::timeout(
        std::time::Duration::from_secs(60),
        async {
            loop {
                let rows = observer.query("SELECT wait_event_type = 'Lock' AND query LIKE '%test_capture%' FROM pg_stat_activity WHERE pid = $1", &[&pid]).await.unwrap();
                if rows[0].get::<_, Option<bool>>(0) == Some(true) {
                    break;
                }
                observer.rollback().await.unwrap();
                tokio::task::yield_now().await;
            }
        },
    ));
    tokio::select! {
        outcome = &mut creating => panic!("create passed the business INSERT lock: {outcome:?}"),
        outcome = &mut waiting => outcome.expect("business INSERT never reached the lock barrier"),
    }
    drop(waiting);
    drop(creating);
    blocker.rollback().await.unwrap();
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "the transaction can no longer be used: an ORM operation was cancelled (its future was dropped) while it was changing the transaction's state; roll back"
    );
    tx.rollback().await.unwrap();
    observer.rollback().await.unwrap();
    assert_eq!(count(&mut observer, "test_capture").await, 0);
    assert_eq!(count(&mut observer, TABLE).await, 0);
    observer.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

type CheckFn = erp_orm::MethodFn<(), ()>;
fn reject_constraint<'a>(
    _: &'a mut Transaction,
    _: &'a erp_orm::Records,
    _: (),
    _: erp_orm::Super<(), ()>,
) -> erp_orm::BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async { Err(Error::validation("capture fixture constraint refusal")) })
}
fn constraint_entry() -> &'static (dyn std::any::Any + Send + Sync) {
    static METHOD: CheckFn = reject_constraint;
    &METHOD
}
static CONSTRAINT_METHOD: erp_orm::MethodImpl = erp_orm::MethodImpl {
    addon: "test_capture",
    model: EVENTFUL,
    method: "check_capture",
    kind: erp_orm::ImplKind::Define,
    sig_name: std::any::type_name::<CheckFn>,
    entry: constraint_entry,
    file: file!(),
    line: line!(),
};
static CONSTRAINT: erp_orm::ConstraintImpl = erp_orm::ConstraintImpl {
    addon: "test_capture",
    model: EVENTFUL,
    method: "check_capture",
    fields: &["number"],
    file: file!(),
    line: line!(),
};

#[tokio::test]
async fn direct_create_dispatch_failure_is_rollback_only() {
    use erp_domain::{Domain, Operator, Value as DomainValue};
    use erp_testkit::{DomainPolicy, Otherwise};
    for constraint in [false, true] {
        let domain = Domain::condition("number", Operator::Gt, DomainValue::Int(0)).unwrap();
        let policy = DomainPolicy::builder(Otherwise::Allow)
            .rule(EVENTFUL, erp_orm::Operation::Create, domain)
            .build()
            .unwrap();
        let mut builder =
            Harness::builder([&base_users::ADDON, &ADDON]).with_decls([ModelDecl::new(
                "test_capture",
                EVENTFUL,
            )
            .description("Capture")
            .eventful(true)
            .fields(const { &[FieldDecl::char("name"), FieldDecl::integer("number")] })]);
        if constraint {
            builder = builder
                .with_impls([&CONSTRAINT_METHOD])
                .with_constraints([&CONSTRAINT]);
        } else {
            builder = builder.policy(policy);
        }
        let h = builder.build().await.unwrap();
        let mut setup = h.begin().await.unwrap();
        setup.install_event_schema().await.unwrap();
        setup.execute("INSERT INTO res_users (id,name,login,active) VALUES (2,'capture user','capture-user',true)", &[]).await.unwrap();
        setup.commit().await.unwrap();
        let mut tx = h
            .orm()
            .begin_opts(
                BeginOptions::new(erp_orm::UserId::new(2).unwrap())
                    .operation_identity(OperationIdentity::generate().unwrap()),
            )
            .await
            .unwrap();
        let records = tx.env().records(EVENTFUL).unwrap();
        let error = erp_orm::call_method::<(Vec<RawVals>,), erp_orm::Records>(
            &mut tx,
            &records,
            "create",
            (vec![RawVals::new().set_cell("number", Cell::Int(-1))],),
        )
        .await
        .unwrap_err();
        let ids = tx.query("SELECT id FROM test_capture", &[]).await.unwrap();
        assert_eq!(ids.len(), 1, "the failure must follow the business INSERT");
        let kind = if constraint {
            ErrorKind::Validation
        } else {
            ErrorKind::Access
        };
        let message = if constraint {
            "capture fixture constraint refusal".to_owned()
        } else {
            format!(
                "You are not allowed to create 'Capture' (test.capture) records.\n\nRecords: test.capture({},), User: 2",
                ids[0].get::<_, i64>(0)
            )
        };
        assert_eq!(error.kind(), kind);
        assert_eq!(error.message(), message);
        let commit_error = tx.commit().await.unwrap_err();
        assert_eq!(commit_error.kind(), kind);
        assert_eq!(commit_error.message(), message);
        tx.rollback().await.unwrap();
        let mut check = h.begin().await.unwrap();
        assert_eq!(count(&mut check, "test_capture").await, 0);
        assert_eq!(count(&mut check, TABLE).await, 0);
        check.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

type UpsertFn = erp_orm::MethodFn<(erp_orm::UpsertCall,), erp_orm::UpsertResult>;
static UPSERT_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn upsert_override<'a>(
    tx: &'a mut Transaction,
    _: &'a erp_orm::Records,
    _: (erp_orm::UpsertCall,),
    _: erp_orm::Super<(erp_orm::UpsertCall,), erp_orm::UpsertResult>,
) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_orm::UpsertResult>> {
    Box::pin(async move {
        UPSERT_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tx.execute(
            "INSERT INTO test_capture_plain (name) VALUES ('override ran')",
            &[],
        )
        .await?;
        Err(Error::programming("capture fixture upsert override ran"))
    })
}
fn upsert_entry() -> &'static (dyn std::any::Any + Send + Sync) {
    static METHOD: UpsertFn = upsert_override;
    &METHOD
}
static UPSERT_OVERRIDE: erp_orm::MethodImpl = erp_orm::MethodImpl {
    addon: "test_capture",
    model: EVENTFUL,
    method: "upsert",
    kind: erp_orm::ImplKind::Override,
    sig_name: std::any::type_name::<UpsertFn>,
    entry: upsert_entry,
    file: file!(),
    line: line!(),
};

#[tokio::test]
async fn upsert_preflight_does_not_dispatch_a_mutating_override() {
    UPSERT_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new("test_capture", EVENTFUL)
                .eventful(true)
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("test_capture", PLAIN).fields(const { &[FieldDecl::char("name")] }),
        ])
        .with_impls([&UPSERT_OVERRIDE])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .env()
        .records(EVENTFUL)
        .unwrap()
        .upsert(
            &mut tx,
            erp_orm::UpsertCall::new("name")
                .with_rows([RawVals::new().set_cell("name", Cell::Str("not dispatched".into()))]),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "outbox: eventful mutation requires an OperationIdentity"
    );
    assert_eq!(UPSERT_CALLS.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(count(&mut tx, "test_capture_plain").await, 0);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn delegated_create_keeps_parent_and_child_capture_independent() {
    const PARENT: &str = "test.capture.parent";
    for parent_eventful in [false, true] {
        let h = Harness::builder([&base_users::ADDON, &ADDON])
            .with_decls([
                ModelDecl::new("test_capture", PARENT)
                    .eventful(parent_eventful)
                    .fields(const { &[FieldDecl::char("name")] }),
                ModelDecl::new("test_capture", EVENTFUL)
                    .eventful(!parent_eventful)
                    .inherits(&[(PARENT, "parent_id")])
                    .fields(
                        const {
                            &[
                                FieldDecl::many2one("parent_id", PARENT)
                                    .required(true)
                                    .ondelete(erp_fields::OnDelete::Cascade),
                                FieldDecl::integer("number"),
                            ]
                        },
                    ),
            ])
            .build()
            .await
            .unwrap();
        let mut tx = begin(&h).await;
        tx.install_event_schema().await.unwrap();
        let made = tx
            .env()
            .records(EVENTFUL)
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("delegated".into()))
                        .set_cell("number", Cell::Int(7)),
                ],
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut check = h.begin().await.unwrap();
        let rows = check
            .query(
                &format!("SELECT model, field_names, record_ids::text FROM {TABLE}"),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let expected_model = if parent_eventful { PARENT } else { EVENTFUL };
        assert_eq!(rows[0].get::<_, String>(0), expected_model);
        let names: Vec<String> = rows[0].get(1);
        assert_eq!(names.iter().any(|n| n == "name"), parent_eventful);
        assert_eq!(names.iter().any(|n| n == "number"), !parent_eventful);
        let captured: Vec<i64> = serde_json::from_str(rows[0].get::<_, &str>(2)).unwrap();
        let actual = if parent_eventful {
            check
                .query(
                    "SELECT parent_id FROM test_capture WHERE id = $1",
                    &[&made.ids()[0]],
                )
                .await
                .unwrap()[0]
                .get::<_, i64>(0)
        } else {
            made.ids()[0]
        };
        assert_eq!(captured, vec![actual]);
        check.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn serialization_failed_attempt_does_not_leave_an_event_on_retry() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let plain = setup
        .env()
        .records(PLAIN)
        .unwrap()
        .create(
            &mut setup,
            vec![RawVals::new().set_cell("number", Cell::Int(0))],
        )
        .await
        .unwrap()
        .ids()[0];
    setup.commit().await.unwrap();
    drop(setup);
    let identity = OperationIdentity::generate().unwrap();
    let mut first = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).operation_identity(identity.clone()))
        .await
        .unwrap();
    first
        .query(
            "SELECT number FROM test_capture_plain WHERE id=$1",
            &[&plain],
        )
        .await
        .unwrap();
    let mut competitor = h.begin().await.unwrap();
    competitor
        .execute(
            "UPDATE test_capture_plain SET number=1 WHERE id=$1",
            &[&plain],
        )
        .await
        .unwrap();
    competitor.commit().await.unwrap();
    drop(competitor);
    create(&mut first, "discarded attempt").await;
    let error = first
        .execute(
            "UPDATE test_capture_plain SET number=2 WHERE id=$1",
            &[&plain],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency);
    assert_eq!(error.sqlstate(), Some("40001"));
    assert_eq!(
        error.message(),
        "could not serialize access due to concurrent update"
    );
    first.rollback().await.unwrap();
    drop(first);
    let mut retry = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).operation_identity(identity.clone()))
        .await
        .unwrap();
    let kept = create(&mut retry, "kept attempt").await;
    retry
        .execute(
            "UPDATE test_capture_plain SET number=2 WHERE id=$1",
            &[&plain],
        )
        .await
        .unwrap();
    retry.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let rows = check
        .query(
            &format!("SELECT record_ids::text, operation_id::text FROM {TABLE}"),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        serde_json::from_str::<Vec<i64>>(rows[0].get::<_, &str>(0)).unwrap(),
        kept
    );
    assert_eq!(rows[0].get::<_, String>(1), identity.operation().as_str());
    assert_eq!(count(&mut check, "test_capture").await, 1);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn oversized_capture_cannot_commit_business_rows_after_ignored_error() {
    let fields = Box::leak(
        (0..129)
            .map(|n| FieldDecl::integer(Box::leak(format!("f{n}").into_boxed_str())))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("test_capture", EVENTFUL)
            .eventful(true)
            .fields(fields)])
        .build()
        .await
        .unwrap();
    let mut tx = begin(&h).await;
    tx.install_event_schema().await.unwrap();
    tx.commit().await.unwrap();
    let mut values = RawVals::new();
    for n in 0..129 {
        values.insert(format!("f{n}").into(), Cell::Int(n));
    }
    let records = tx.env().records(EVENTFUL).unwrap();
    let error = erp_orm::call_method::<(Vec<RawVals>,), erp_orm::Records>(
        &mut tx,
        &records,
        "create",
        (vec![values],),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "outbox CaptureLimit: event exceeds 128 changed field names"
    );
    assert_eq!(
        count(&mut tx, "test_capture").await,
        1,
        "admission error must follow INSERT"
    );
    let commit = tx.commit().await.unwrap_err();
    assert_eq!(commit.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(commit.message(), error.message());
    tx.rollback().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert_eq!(count(&mut check, "test_capture").await, 0);
    assert_eq!(count(&mut check, TABLE).await, 0);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn aborted_transaction_never_inserts_queued_events() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    create(&mut tx, "aborted").await;
    tx.capture_statements(true);
    let error = tx.query("SELECT 1/0", &[]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), "division by zero");
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("22012"));
    assert_eq!(
        error.message(),
        "transaction rolled back: an earlier statement failed (division by zero)"
    );
    assert_eq!(inserts(&tx), 0);
    let mut check = h.begin().await.unwrap();
    assert_eq!(count(&mut check, "test_capture").await, 0);
    assert_eq!(count(&mut check, TABLE).await, 0);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn capture_insert_uses_only_runtime_supplied_column_privileges() {
    let h = harness().await;
    let role = format!("capture_{}", h.db().name());
    let quoted = erp_registry::ident::quote(&role);
    let mut setup = h.begin().await.unwrap();
    setup
        .execute(&format!("CREATE ROLE {quoted} NOLOGIN"), &[])
        .await
        .unwrap();
    setup.grant_event_runtime_role(&role).await.unwrap();
    setup
        .execute(
            &format!("GRANT SELECT, INSERT ON test_capture TO {quoted}"),
            &[],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!("GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO {quoted}"),
            &[],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = begin(&h).await;
    tx.execute(&format!("SET LOCAL ROLE {quoted}"), &[])
        .await
        .unwrap();
    assert_eq!(
        tx.query("SELECT current_user::text", &[]).await.unwrap()[0].get::<_, String>(0),
        role
    );
    let made = create(&mut tx, "runtime writer").await;
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let rows = check.query(&format!("SELECT record_ids::text, event_id IS NOT NULL AND xid IS NOT NULL AND create_date IS NOT NULL FROM {TABLE}"), &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        serde_json::from_str::<Vec<i64>>(rows[0].get::<_, &str>(0)).unwrap(),
        made
    );
    assert!(rows[0].get::<_, bool>(1));
    check
        .execute(&format!("DROP OWNED BY {quoted}"), &[])
        .await
        .unwrap();
    check
        .execute(&format!("DROP ROLE {quoted}"), &[])
        .await
        .unwrap();
    check.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn missing_partition_at_finalization_rolls_back_business_create() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let parts = setup.query("SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid=i.inhrelid WHERE i.inhparent='erp_event.ir_event_outbox'::regclass", &[]).await.unwrap();
    assert!(!parts.is_empty());
    let names: Vec<String> = parts
        .iter()
        .map(|row| {
            format!(
                "\"erp_event\".{}",
                erp_registry::ident::quote(row.get::<_, &str>(0))
            )
        })
        .collect();
    setup
        .execute(&format!("DROP TABLE {}", names.join(", ")), &[])
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let mut tx = begin(&h).await;
    create(&mut tx, "no partition").await;
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.sqlstate(), Some("23514"));
    // PostgreSQL appends the partition-key timestamp in detail; the primary message is stable.
    assert_eq!(
        error.message(),
        "The operation cannot be completed: no partition of relation \"ir_event_outbox\" found for row"
    );
    let mut check = h.begin().await.unwrap();
    assert_eq!(count(&mut check, "test_capture").await, 0);
    assert_eq!(count(&mut check, TABLE).await, 0);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
