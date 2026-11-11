//! Bounded S7 acceptance: delegated capture attribution, access refusals and rollback.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::num::NonZeroU64;

use erp_domain::{Domain, Operator};
use erp_fields::{Cell, Error, ErrorKind, OnDelete, Result, Value};
use erp_orm::{
    BeginOptions, CompanyId, FieldOp, GroupSet, Operation, OperationIdentity, Principal,
    PrincipalBinding, PrincipalBuilder, RawVals, Resolution, SUPERUSER_ID, Transaction, UserId,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, ModelSpec};
use erp_test_models::base_users;
use erp_testkit::{FieldPolicy, Harness, TestRules};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_outbox_delegated_access",
    depends: &["base"],
    exact_only: false,
};
const PARENT: &str = "test.outbox.access.parent";
const CHILD: &str = "test.outbox.access.child";
const EVENTS: &str = "\"erp_event\".\"ir_event_outbox\"";
const REJECTION: &str = "delegated fixture rejects child after parent routing";

#[derive(Clone, Copy)]
enum Denial {
    None,
    ChildField,
    ParentField,
    ChildModel,
    ParentModel,
    ChildRecord,
    ParentRecord,
    ReassignmentTarget,
}

struct Rules(Denial);
impl TestRules for Rules {
    fn check_model(
        &self,
        _: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        if op == Operation::Write
            && ((matches!(self.0, Denial::ChildModel) && model.name == CHILD)
                || (matches!(self.0, Denial::ParentModel) && model.name == PARENT))
        {
            return Err(Error::access(format!(
                "delegated fixture denies write on {}",
                model.name
            )));
        }
        Ok(())
    }

    fn access_domain(
        &self,
        _: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<Domain> {
        if op == Operation::Write {
            match self.0 {
                Denial::ChildRecord if model.name == CHILD => {
                    return Ok(Domain::condition("note", Operator::Ne, "Child initial")?);
                }
                Denial::ParentRecord if model.name == PARENT => {
                    return Ok(Domain::condition("name", Operator::Ne, "Parent initial")?);
                }
                Denial::ReassignmentTarget if model.name == PARENT => {
                    return Ok(Domain::condition("name", Operator::Ne, "Target initial")?);
                }
                // These cases impose no record predicate; model/field checks own the refusal.
                _ => {}
            }
        }
        Ok(Domain::TRUE)
    }
}

type CheckFn = erp_orm::MethodFn<(), ()>;
fn child_constraint<'a>(
    tx: &'a mut Transaction,
    records: &'a erp_orm::Records,
    _: (),
    _: erp_orm::Super<(), ()>,
) -> erp_orm::BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        for id in records.ids() {
            let record = records.browse([id])?;
            if record.value(tx, "note").await? == Value::str("reject") {
                // Verify the failure really occurs after the inherited write became visible.
                if record.value(tx, "name").await? != Value::str("Parent staged") {
                    return Err(Error::programming(
                        "constraint did not observe routed parent change",
                    ));
                }
                return Err(Error::validation(REJECTION));
            }
        }
        Ok(())
    })
}
fn constraint_entry() -> &'static (dyn std::any::Any + Send + Sync) {
    static METHOD: CheckFn = child_constraint;
    &METHOD
}
static CONSTRAINT_METHOD: erp_orm::MethodImpl = erp_orm::MethodImpl {
    addon: ADDON.name,
    model: CHILD,
    method: "check_delegated_child",
    kind: erp_orm::ImplKind::Define,
    sig_name: std::any::type_name::<CheckFn>,
    entry: constraint_entry,
    file: file!(),
    line: line!(),
};
static CONSTRAINT: erp_orm::ConstraintImpl = erp_orm::ConstraintImpl {
    addon: ADDON.name,
    model: CHILD,
    method: "check_delegated_child",
    fields: &["name", "note"],
    file: file!(),
    line: line!(),
};

async fn harness(denial: Denial) -> Harness {
    let builder = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new(ADDON.name, PARENT)
                .table("test_outbox_access_parent")
                .description("Delegated access parent")
                .eventful(true)
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new(ADDON.name, CHILD)
                .table("test_outbox_access_child")
                .description("Delegated access child")
                .eventful(true)
                .inherits(&[(PARENT, "parent_id")])
                .fields(
                    const {
                        &[
                            FieldDecl::many2one("parent_id", PARENT)
                                .required(true)
                                .ondelete(OnDelete::Cascade),
                            FieldDecl::char("note"),
                        ]
                    },
                ),
        ])
        .with_impls([&CONSTRAINT_METHOD])
        .with_constraints([&CONSTRAINT]);
    let builder = match denial {
        Denial::ChildField => {
            builder.policy(FieldPolicy::new().deny(CHILD, "note", FieldOp::Write))
        }
        Denial::ParentField => {
            builder.policy(FieldPolicy::new().deny(PARENT, "name", FieldOp::Write))
        }
        _ => builder.rules(Rules(denial)),
    };
    builder.build().await.unwrap()
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

// These hand-built resolutions belong to the test policy, not the database resolver suite.
fn principal(uid: i64, companies: &[i64]) -> Principal {
    let companies: Vec<_> = companies
        .iter()
        .map(|id| CompanyId::new(*id).unwrap())
        .collect();
    PrincipalBuilder::user(UserId::new(uid).unwrap())
        .resolution(
            Resolution::in_id_order(GroupSet::default(), companies.clone(), NonZeroU64::MIN)
                .unwrap(),
        )
        .companies(companies)
        .build()
        .unwrap()
}

async fn user_transaction(h: &Harness, uid: i64, companies: &[i64]) -> Transaction {
    h.orm()
        .begin_opts(
            BeginOptions::for_principal(principal(uid, companies))
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
}

#[derive(Debug, PartialEq, Eq)]
struct State {
    parents: Vec<(i64, String)>,
    children: Vec<(i64, i64, String)>,
    event_count: i64,
}

async fn state(h: &Harness) -> State {
    let mut check = h.begin().await.unwrap();
    let parents = check
        .query(
            "SELECT id, name FROM test_outbox_access_parent ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let children = check
        .query(
            "SELECT id, parent_id, note FROM test_outbox_access_child ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    let event_count = check
        .query(&format!("SELECT count(*) FROM {EVENTS}"), &[])
        .await
        .unwrap()[0]
        .get(0);
    check.rollback().await.unwrap();
    State {
        parents,
        children,
        event_count,
    }
}

struct Fixture {
    parent: i64,
    target: i64,
    child: i64,
}
async fn seed(h: &Harness) -> Fixture {
    let mut tx = begin(h).await;
    tx.install_event_schema().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES \
        (7, 'Writer', 'delegated-writer', true), \
        (8, 'Flusher', 'delegated-flusher', true), \
        (9, 'Subject', 'delegated-subject', true)",
        &[],
    )
    .await
    .unwrap();
    let parents = tx
        .env()
        .records(PARENT)
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new().set_cell("name", Cell::Str("Parent initial".into())),
                RawVals::new().set_cell("name", Cell::Str("Target initial".into())),
            ],
        )
        .await
        .unwrap()
        .ids();
    let child = tx
        .env()
        .records(CHILD)
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("parent_id", Cell::Id(parents[0]))
                    .set_cell("note", Cell::Str("Child initial".into())),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    Fixture {
        parent: parents[0],
        target: parents[1],
        child,
    }
}

fn mixed(name: &str, note: &str) -> RawVals {
    RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("note", Cell::Str(note.into()))
}

async fn events(h: &Harness, operation: &str) -> Vec<tokio_postgres::Row> {
    let mut check = h.begin().await.unwrap();
    let rows = check.query(&format!("SELECT model, record_ids, field_names, subject, actor_kind, \
        actor_id, delegating_user_id, company_ids, principal_kind, operation_id::text, correlation_id::text \
        FROM {EVENTS} WHERE operation_id::text = $1 AND kind = 'write' ORDER BY id"), &[&operation])
        .await.unwrap();
    check.rollback().await.unwrap();
    rows
}

fn assert_error(error: &Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind, "{error}");
    assert_eq!(error.message(), message);
}

#[tokio::test]
async fn writer_and_impersonator_attribution_survive_a_different_flusher() {
    for impersonate in [false, true] {
        let h = harness(Denial::None).await;
        let fx = seed(&h).await;
        let mut origin = user_transaction(&h, 7, &[1, 2]).await;
        let writer = if impersonate {
            // An explicit impersonation path; it is deliberately separate from an ordinary user.
            origin.env().with_user(9)
        } else {
            origin.env().clone()
        };
        let writer = writer.with_context([("allowed_company_ids", Value::list([Value::Int(2)]))]);
        let operation = writer.operation_identity().unwrap().operation().to_string();
        let correlation = writer
            .operation_identity()
            .unwrap()
            .correlation()
            .to_string();
        let subject = writer.uid().to_string();
        assert!(!writer.su(), "authority coverage must not use sudo");
        origin.rollback().await.unwrap();
        drop(origin);
        let mut tx = user_transaction(&h, 8, &[3]).await;
        assert_ne!(
            operation,
            tx.env()
                .operation_identity()
                .unwrap()
                .operation()
                .to_string()
        );
        assert_ne!(writer.uid(), tx.env().uid());
        // Use the same validated transaction API as CaptureContext, rather than assuming
        // Env::companies alone is the effective company oracle.
        let companies: Vec<i64> = tx
            .companies(&writer)
            .await
            .unwrap()
            .iter()
            .map(|c| c.get())
            .collect();
        assert_eq!(companies, [2]);
        if impersonate {
            let Some(erp_orm::Actor::Impersonator(actor)) = writer.principal().actor() else {
                panic!("with_user must preserve the original actor");
            };
            assert_eq!(actor.subject().get(), 7);
            assert_eq!(actor.kind(), erp_orm::PrincipalKind::User);
        } else {
            assert!(writer.principal().actor().is_none());
        }
        writer
            .records(CHILD)
            .unwrap()
            .browse([fx.child])
            .unwrap()
            .write(&mut tx, mixed("Parent written", "Child written"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let rows = events(&h, &operation).await;
        assert_eq!(rows.len(), 2);
        for (row, model, id, field) in [
            (&rows[0], PARENT, fx.parent, "name"),
            (&rows[1], CHILD, fx.child, "note"),
        ] {
            assert_eq!(row.get::<_, String>("model"), model);
            assert_eq!(
                row.get::<_, serde_json::Value>("record_ids"),
                serde_json::json!([id])
            );
            assert_eq!(row.get::<_, Vec<String>>("field_names"), [field]);
            assert_eq!(row.get::<_, String>("subject"), subject);
            assert_eq!(row.get::<_, String>("actor_kind"), "user");
            assert_eq!(row.get::<_, String>("principal_kind"), "user");
            assert_eq!(row.get::<_, i64>("actor_id"), 7);
            assert_eq!(
                row.get::<_, Option<i64>>("delegating_user_id"),
                impersonate.then_some(7)
            );
            assert_eq!(row.get::<_, Vec<i64>>("company_ids"), companies);
            assert_eq!(row.get::<_, String>("operation_id"), operation);
            assert_eq!(
                row.get::<_, Option<String>>("correlation_id"),
                Some(correlation.clone())
            );
        }
        let actual = state(&h).await;
        assert_eq!(
            actual.parents,
            [
                (fx.parent, "Parent written".into()),
                (fx.target, "Target initial".into())
            ]
        );
        assert_eq!(
            actual.children,
            [(fx.child, fx.parent, "Child written".into())]
        );
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn denied_mixed_writes_preserve_both_owners_and_events() {
    for denial in [
        Denial::ChildField,
        Denial::ParentField,
        Denial::ChildModel,
        Denial::ParentModel,
        Denial::ChildRecord,
        Denial::ParentRecord,
        Denial::ReassignmentTarget,
    ] {
        let h = harness(denial).await;
        let fx = seed(&h).await;
        let before = state(&h).await;
        let mut tx = user_transaction(&h, 7, &[]).await;
        let mut vals = mixed("Unauthorized parent", "Unauthorized child");
        if matches!(denial, Denial::ReassignmentTarget) {
            vals = vals.set_cell("parent_id", Cell::Id(fx.target));
        }
        let error = tx
            .env()
            .records(CHILD)
            .unwrap()
            .browse([fx.child])
            .unwrap()
            .write(&mut tx, vals)
            .await
            .unwrap_err();
        let expected = match denial {
            Denial::ChildField | Denial::ParentField => {
                let (model, description, field) = if matches!(denial, Denial::ChildField) {
                    (CHILD, "Delegated access child", "note")
                } else {
                    (PARENT, "Delegated access parent", "name")
                };
                format!(
                    "You do not have enough rights to access the field \"{field}\" on {description} ({model}). Please contact your system administrator.\n\nOperation: write"
                )
            }
            Denial::ChildModel => format!("delegated fixture denies write on {CHILD}"),
            Denial::ParentModel => format!("delegated fixture denies write on {PARENT}"),
            Denial::ChildRecord | Denial::ParentRecord | Denial::ReassignmentTarget => {
                let (model, description, id) = match denial {
                    Denial::ChildRecord => (CHILD, "Delegated access child", fx.child),
                    Denial::ParentRecord => (PARENT, "Delegated access parent", fx.parent),
                    Denial::ReassignmentTarget => (PARENT, "Delegated access parent", fx.target),
                    _ => unreachable!("record denial cases only"),
                };
                format!(
                    "You are not allowed to modify '{description}' ({model}) records.\n\nRecords: {model}({id},), User: 7"
                )
            }
            Denial::None => unreachable!("only denial cases are exercised"),
        };
        assert_error(&error, ErrorKind::Access, &expected);
        // A pre-mutation refusal must leave the transaction usable, including finalization.
        assert_eq!(
            tx.query("SELECT 1::bigint", &[]).await.unwrap()[0].get::<_, i64>(0),
            1
        );
        tx.commit().await.unwrap();
        drop(tx);
        assert_eq!(state(&h).await, before);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn caught_child_constraint_cannot_commit_a_routed_parent_change() {
    let h = harness(Denial::None).await;
    let fx = seed(&h).await;
    let before = state(&h).await;
    let mut tx = user_transaction(&h, 7, &[]).await;
    let error = tx
        .env()
        .records(CHILD)
        .unwrap()
        .browse([fx.child])
        .unwrap()
        .write(&mut tx, mixed("Parent staged", "reject"))
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Validation, REJECTION);
    let commit = tx.commit().await.unwrap_err();
    assert_error(&commit, ErrorKind::Validation, REJECTION);
    assert_eq!(
        state(&h).await,
        before,
        "separate connection sees no partial mutation"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(state(&h).await, before);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rolled_back_constraint_savepoint_allows_a_valid_sibling_write() {
    let h = harness(Denial::None).await;
    let fx = seed(&h).await;
    let mut tx = user_transaction(&h, 7, &[]).await;
    let operation = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    let child = tx.env().records(CHILD).unwrap().browse([fx.child]).unwrap();
    let error = tx
        .savepoint(async |tx| child.write(tx, mixed("Parent staged", "reject")).await)
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Validation, REJECTION);
    assert_eq!(
        child.value(&mut tx, "name").await.unwrap(),
        Value::str("Parent initial")
    );
    assert_eq!(
        child.value(&mut tx, "note").await.unwrap(),
        Value::str("Child initial")
    );
    tx.savepoint(async |tx| {
        child
            .write(
                tx,
                mixed("Parent sibling", "Child sibling").set_cell("parent_id", Cell::Id(fx.target)),
            )
            .await
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let actual = state(&h).await;
    assert_eq!(
        actual.parents,
        [
            (fx.parent, "Parent initial".into()),
            (fx.target, "Parent sibling".into())
        ]
    );
    assert_eq!(
        actual.children,
        [(fx.child, fx.target, "Child sibling".into())]
    );
    let rows = events(&h, &operation).await;
    assert_eq!(
        rows.len(),
        2,
        "rolled-back capture must not survive into the sibling"
    );
    for (row, model, id, fields) in [
        (&rows[0], PARENT, fx.target, &["name"][..]),
        (&rows[1], CHILD, fx.child, &["note", "parent_id"][..]),
    ] {
        assert_eq!(row.get::<_, String>("model"), model);
        assert_eq!(
            row.get::<_, serde_json::Value>("record_ids"),
            serde_json::json!([id])
        );
        assert_eq!(row.get::<_, Vec<String>>("field_names"), fields);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn missing_identity_refuses_delegated_write_before_mutation() {
    let h = harness(Denial::None).await;
    let fx = seed(&h).await;
    let before = state(&h).await;
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal(7, &[])))
        .await
        .unwrap();
    let error = tx
        .env()
        .records(CHILD)
        .unwrap()
        .browse([fx.child])
        .unwrap()
        .write(&mut tx, mixed("Unauthorized parent", "Unauthorized child"))
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        "outbox: eventful mutation requires an OperationIdentity",
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(state(&h).await, before);
    h.cleanup().await.unwrap();
}
