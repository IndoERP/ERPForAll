// SPDX-License-Identifier: MIT OR Apache-2.0
//! Scoped elevation blocks (slice P1): the block lifetime, its
//! identity, the scope validated against the transaction's registry, nested blocks, the elevation
//! recorded before the body, the read-only refusal and the A5 flush rule. Every test runs against
//! the real audit table the harness installs.
//!
//! What a scope grants under the compiled policy, and which lifts it accepts, are the tests of
//! `principal_policy.rs`. Bodies here run under the harness's
//! `StaticPolicy::AllowAll`, which decides without looking at the elevation and, having no
//! restriction rules, refuses every lifted rule at entry (`AccessPolicy::check_scope`'s default).
//! These tests prove the lifetime, never what a scope grants.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::{Error, ErrorKind, Value};
use erp_orm::audit::{ElevationReason, SuperuserAction};
use erp_orm::jobs::{EnqueueRequest, JobKey, JobRef};
use erp_orm::{
    ApiKeyId, BeginOptions, BlockId, CommitError, CommitFault, CredentialOptions, Elevation,
    ElevationScope, Env, FieldAllow, MethodAllow, ModelAllow, Operation, OperationIdentity,
    OperationSet, PrincipalBuilder, PrincipalKind, QualifiedXmlId, ReadAuditOwner, Records,
    SUPERUSER_ID, Scope, ScopeItem, Transaction, UserId,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, NoTls};
use futures_util::FutureExt;

mod fixture {
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "prin_block");

    #[model(
        name = "prin.block.tag",
        description = "Block Tag",
        addon = "prin_block"
    )]
    pub struct Tag {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }

    #[model(
        name = "prin.block.note",
        description = "Block Note",
        addon = "prin_block",
        constraints = [("_size_not_negative", "CHECK(size >= 0)", "The size must not be negative")]
    )]
    pub struct Note {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(integer)]
        pub size: i32,
        #[field(many2one, comodel = "prin.block.tag")]
        pub tag_id: Option<RecId>,
    }
}

/// Only composed into a second registry: its ids are foreign to the harness's registry.
mod foreign {
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "prin_block_foreign");

    #[model(
        name = "prin.block.wide",
        description = "Wide",
        addon = "prin_block_foreign"
    )]
    pub struct Wide {
        #[field(char)]
        pub a1: Option<Arc<str>>,
        #[field(char)]
        pub a2: Option<Arc<str>>,
        #[field(char)]
        pub a3: Option<Arc<str>>,
        #[field(char)]
        pub a4: Option<Arc<str>>,
        #[field(char)]
        pub a5: Option<Arc<str>>,
        #[field(char)]
        pub a6: Option<Arc<str>>,
        #[field(char)]
        pub a7: Option<Arc<str>>,
        #[field(char)]
        pub a8: Option<Arc<str>>,
        #[field(char)]
        pub a9: Option<Arc<str>>,
        #[field(char)]
        pub a10: Option<Arc<str>>,
        #[field(char)]
        pub a11: Option<Arc<str>>,
        #[field(char)]
        pub a12: Option<Arc<str>>,
    }

    #[model(
        name = "prin.block.extra1",
        description = "Extra 1",
        addon = "prin_block_foreign"
    )]
    pub struct Extra1 {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }

    #[model(
        name = "prin.block.extra2",
        description = "Extra 2",
        addon = "prin_block_foreign"
    )]
    pub struct Extra2 {
        #[field(char)]
        pub name: Option<Arc<str>>,
    }
}

/// A company-dependent field with a named precision: the flush validates it under the writer's
/// environment (`write.rs`, the property origins), the case the A5 flush rule is about.
mod property {
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "prin_block_property", depends = ["base"]);

    #[model(
        name = "prin.block.property",
        description = "Block Property",
        addon = "prin_block_property"
    )]
    pub struct Property {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(float, digits = "Prin Block Precision", company_dependent = true)]
        pub named: Decimal,
    }
}

const NOTE: &str = "prin.block.note";
const TAG: &str = "prin.block.tag";
const REASON: &str = "prin_block.test.block";
const OUTSIDE: &str = "elevated environment of block";
const READ_ONLY_REFUSAL: &str = "scoped elevation requires an audited transaction: this read-only \
                                 transaction has no read-audit owner";
const CANCELLED: &str = "the transaction can no longer be used: an ORM operation was cancelled \
                         (its future was dropped) while it was changing the transaction's state; \
                         roll back";
/// The refusal of a lifted rule by a policy without restriction rules (`AllowAll`).
const LIFT_REFUSAL: &str = "cannot lift restriction rule 'prin_block.rule': this access policy has \
                            no restriction rules to lift";

async fn harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON, &fixture::ADDON])
        // user 51 opens a block from its switch to user 52: `AllowAll` grants every capability
        // once the ORM names the impersonation one (A4)
        .impersonate_capability(&erp_security::principal::PRINCIPAL_IMPERSONATE)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    // the users of the scenarios exist, so what they write can name them as its author
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES \
         (51, 'u51', 'u51', true), (52, 'u52', 'u52', true)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

fn user(n: i64) -> UserId {
    UserId::new(n).unwrap()
}

fn reason() -> ElevationReason {
    ElevationReason::new(REASON).unwrap()
}

fn identity() -> OperationIdentity {
    OperationIdentity::generate().unwrap()
}

/// A writable transaction of user `uid` (an unresolved user principal under `AllowAll`).
async fn begin_user(h: &Harness, uid: i64) -> Transaction {
    let principal = PrincipalBuilder::user(user(uid)).build().unwrap();
    h.orm()
        .begin_opts(BeginOptions::for_principal(principal).operation_identity(identity()))
        .await
        .unwrap()
}

const OWNER: &str = "prin-block-test";

/// A read-only transaction of user 51, with or without a declared read-audit owner.
async fn begin_read_only(h: &Harness, owned: bool) -> Transaction {
    let principal = PrincipalBuilder::user(user(51)).build().unwrap();
    let options = BeginOptions::for_principal(principal).operation_identity(identity());
    let options = if owned {
        options.read_only_audited("prin.block.note.read", ReadAuditOwner::new(OWNER).unwrap())
    } else {
        options.read_only("prin.block.note.read")
    };
    h.orm().begin_opts(options).await.unwrap()
}

fn vals(items: &[(&str, Value)]) -> BTreeMap<String, Value> {
    items
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn model(tx: &Transaction, name: &str) -> erp_registry::ModelId {
    tx.registry().model_id(name).unwrap()
}

/// A scope opening `ops` on `name`, every field.
fn scope(tx: &Transaction, name: &str, ops: &[Operation]) -> ElevationScope {
    ElevationScope::new([
        ScopeItem::new(model(tx, name), OperationSet::of(ops.iter().copied())).unwrap(),
    ])
    .unwrap()
}

fn read_write(tx: &Transaction) -> ElevationScope {
    scope(tx, NOTE, &[Operation::Read, Operation::Write])
}

/// A tag and a note pointing at it, created by the transaction's own principal.
async fn seed_note(tx: &mut Transaction) -> (Records, Records) {
    let env = tx.env();
    let tag = env
        .records(TAG)
        .unwrap()
        .create_values(tx, &[vals(&[("name", Value::str("t1"))])])
        .await
        .unwrap();
    let note = env
        .records(NOTE)
        .unwrap()
        .create_values(
            tx,
            &[vals(&[
                ("name", Value::str("n1")),
                ("size", Value::Int(3)),
                ("tag_id", Value::Int(tag.ids()[0])),
            ])],
        )
        .await
        .unwrap();
    (tag, note)
}

fn block_of(env: &Env) -> BlockId {
    match env.principal().elevation() {
        Elevation::Scoped { block, .. } => *block,
        other => panic!("not a scoped environment: {other:?}"),
    }
}

/// The lifetime refusal for the block of `block`, built from the block id our code assigned.
fn outside(block: BlockId) -> String {
    format!(
        "{OUTSIDE} {}#{} (reason '{REASON}') used outside it",
        block.tx().get(),
        block.seq()
    )
}

fn widened(block: BlockId) -> String {
    format!(
        "an environment of elevation block {}#{} (reason '{REASON}') cannot be widened to \
         superuser mode",
        block.tx().get(),
        block.seq()
    )
}

#[track_caller]
fn assert_error(error: &Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind, "{error:?}");
    assert_eq!(error.message(), message);
}

/// An owner connection (the owner role holds every right on the audit table).
async fn owner(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    client
}

/// The events of the only audit row.
async fn events(h: &Harness) -> Vec<serde_json::Value> {
    let text: String = owner(h)
        .await
        .query_one("SELECT events::text FROM erp_event.ir_audit_entry", &[])
        .await
        .unwrap()
        .get(0);
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value.as_array().unwrap().clone()
}

/// another transaction, whatever the cache holds: scalar, id, relational and fetched values were
/// read by the base environment in the same transaction first. An ordinary permitted read still
/// succeeds.
#[tokio::test]
async fn escaped_environments_and_records_are_refused() {
    let h = harness().await;
    let mut tx = begin_user(&h, 51).await;
    let (_, note) = seed_note(&mut tx).await;
    // warm the cache with the base environment: scalar, id, relational and a fetch
    let name = note.value(&mut tx, "name").await.unwrap();
    let id = note.cell(&mut tx, "id").await.unwrap();
    let tag = note.related(&mut tx, "tag_id").await.unwrap();
    note.fetch(&mut tx, Some(&["name", "size", "tag_id"]))
        .await
        .unwrap();
    let base = tx.env();
    let s = read_write(&tx);
    let (env, records) = tx
        .elevated(&base, reason(), s, async |tx, env| {
            // inside the block the escaped values are usable
            let inside = note.with_env(env.clone());
            assert_eq!(inside.value(tx, "name").await?, name);
            Ok((env, inside))
        })
        .await
        .unwrap();
    let message = outside(block_of(&env));
    assert_error(
        &records.value(&mut tx, "name").await.unwrap_err(),
        ErrorKind::Programming,
        &message,
    );
    assert_error(
        &records.cell(&mut tx, "id").await.unwrap_err(),
        ErrorKind::Programming,
        &message,
    );
    assert_error(
        &records.related(&mut tx, "tag_id").await.unwrap_err(),
        ErrorKind::Programming,
        &message,
    );
    assert_error(
        &records
            .fetch(&mut tx, Some(&["name", "size", "tag_id"]))
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        &message,
    );
    assert_error(
        &env.records(NOTE)
            .unwrap()
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        &message,
    );
    // a derived environment keeps the lifetime
    assert_error(
        &note
            .with_env(env.with_context([("lang", Value::str("en_US"))]))
            .value(&mut tx, "name")
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        &message,
    );
    // the ordinary permitted reads still answer, from the same transaction
    assert_eq!(note.value(&mut tx, "name").await.unwrap(), name);
    assert_eq!(note.cell(&mut tx, "id").await.unwrap(), id);
    assert_eq!(
        note.related(&mut tx, "tag_id").await.unwrap().ids(),
        tag.ids()
    );
    // the escaped environment enqueues nothing (refused before the kind is even looked up) and
    // reads no companies
    assert_error(
        &tx.enqueue_many(
            &env,
            vec![EnqueueRequest::new("prin_block.never_registered")],
        )
        .await
        .unwrap_err(),
        ErrorKind::Programming,
        &message,
    );
    assert_error(
        &tx.companies(&env).await.unwrap_err(),
        ErrorKind::Programming,
        &message,
    );
    tx.commit().await.unwrap();
    // and in another transaction the escaped values are refused as well
    let mut other = begin_user(&h, 51).await;
    // also inside a block of that transaction that got the same number: the serials differ
    let escaped_block = block_of(&env);
    let other_base = other.env();
    let s = read_write(&other);
    let (same_number, inside) = other
        .elevated(&other_base, reason(), s, async |tx, own| {
            Ok((block_of(&own), records.value(tx, "name").await))
        })
        .await
        .unwrap();
    assert_eq!(
        same_number.seq(),
        escaped_block.seq(),
        "the same block number"
    );
    assert_ne!(
        same_number.tx(),
        escaped_block.tx(),
        "another transaction serial"
    );
    assert_error(&inside.unwrap_err(), ErrorKind::Programming, &message);
    assert_error(
        &records.value(&mut other, "name").await.unwrap_err(),
        ErrorKind::Programming,
        &message,
    );
    assert_error(
        &env.records(NOTE)
            .unwrap()
            .search(&mut other, &Domain::TRUE)
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        &message,
    );
    let fresh = other
        .env()
        .records(NOTE)
        .unwrap()
        .search(&mut other, &Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(fresh.value(&mut other, "name").await.unwrap(), name);
    drop(other);
    h.cleanup().await.unwrap();
}

/// the body waits, and when the body panics; the escaped environment is refused after each (in
/// another transaction when the first one is cancelled). `Err` leaves the transaction usable; a
/// dropped future or a panic cancels it, as any interrupted scope that changes the transaction
/// does, so a write the body left pending is never flushed later: the later flush and commit are
/// refused with the cancellation error. A transaction cancelled inside the body stays cancelled
/// after the block closed (closing it recovers nothing).
#[tokio::test]
async fn error_drop_and_unwind_close_the_block() {
    let h = harness().await;
    let mut tx = begin_user(&h, 51).await;
    let (_, note) = seed_note(&mut tx).await;
    let base = tx.env();

    // Err: the body's error is the block's error, the transaction stays usable
    let mut escaped: Option<Env> = None;
    let error = tx
        .elevated(&base, reason(), read_write(&tx), async |_tx, env| {
            escaped = Some(env);
            Err::<(), _>(Error::validation("the body refused"))
        })
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Validation, "the body refused");
    let env = escaped.take().unwrap();
    assert_error(
        &note
            .with_env(env.clone())
            .value(&mut tx, "name")
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        &outside(block_of(&env)),
    );
    assert_eq!(note.value(&mut tx, "name").await.unwrap(), Value::str("n1"));
    tx.commit().await.unwrap();

    // a dropped future: the body writes, then waits forever; the timeout drops the future (the
    // wait bounds nothing that is asserted, it only makes the drop happen)
    let mut tx = begin_user(&h, 51).await;
    let mine = note.with_env(tx.env());
    let base = tx.env();
    let s = read_write(&tx);
    let dropped = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tx.elevated(&base, reason(), s, async |tx, env| {
            escaped = Some(env.clone());
            mine.with_env(env)
                .write_values(tx, &vals(&[("name", Value::str("never flushed"))]))
                .await?;
            std::future::pending::<()>().await;
            Ok(())
        }),
    )
    .await;
    assert!(dropped.is_err(), "the body was still waiting");
    assert_error(
        &tx.flush().await.unwrap_err(),
        ErrorKind::Programming,
        CANCELLED,
    );
    assert_error(
        &tx.commit().await.unwrap_err(),
        ErrorKind::Programming,
        CANCELLED,
    );
    drop(tx);
    let env = escaped.take().unwrap();
    let mut other = begin_user(&h, 51).await;
    assert_error(
        &note
            .with_env(env.clone())
            .value(&mut other, "name")
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        &outside(block_of(&env)),
    );
    assert_eq!(
        note.value(&mut other, "name").await.unwrap(),
        Value::str("n1"),
        "the pending write never reached the database"
    );
    drop(other);

    // unwind
    let mut tx = begin_user(&h, 51).await;
    let base = tx.env();
    let s = read_write(&tx);
    let unwound =
        AssertUnwindSafe(
            tx.elevated(&base, reason(), s, async |_tx, env| -> Result<(), Error> {
                escaped = Some(env);
                panic!("the body panicked")
            }),
        )
        .catch_unwind()
        .await;
    assert!(unwound.is_err(), "the body panicked");
    assert_error(
        &tx.flush().await.unwrap_err(),
        ErrorKind::Programming,
        CANCELLED,
    );
    drop(tx);
    let env = escaped.take().unwrap();
    let mut other = begin_user(&h, 51).await;
    assert_error(
        &note
            .with_env(env.clone())
            .value(&mut other, "name")
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        &outside(block_of(&env)),
    );
    drop(other);

    // a transaction cancelled inside the body stays cancelled: closing the block recovers nothing
    let mut tx = begin_user(&h, 51).await;
    let base = tx.env();
    let s = read_write(&tx);
    let error = tx
        .elevated(&base, reason(), s, async |tx, _env| {
            tx.taint_cancelled_for_tests();
            Err::<(), _>(Error::validation("cancelled inside"))
        })
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Validation, "cancelled inside");
    assert_error(
        &note.value(&mut tx, "name").await.unwrap_err(),
        ErrorKind::Programming,
        CANCELLED,
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// environment works inside the inner body and again after it; the inner environment is refused
/// once the inner block closed; a refused inner entry leaves the outer block open. Derived
/// environments keep the lifetime: `sudo(false)` and `with_user(other)` stay bound, `sudo(true)`
/// and `with_user(SUPERUSER_ID)` refuse every use.
#[tokio::test]
async fn nested_blocks_carry_the_inner_scope_and_derivations_keep_the_lifetime() {
    let h = harness().await;
    let mut tx = begin_user(&h, 51).await;
    let (tag, note) = seed_note(&mut tx).await;
    let base = tx.env();
    let outer_scope = read_write(&tx);
    let inner_scope = scope(&tx, TAG, &[Operation::Read]);
    let expected_inner = inner_scope.clone();
    let bad_inner =
        ElevationScope::new([
            ScopeItem::new(model(&tx, TAG), OperationSet::of([Operation::Read]))
                .unwrap()
                .lift([QualifiedXmlId::parse("prin_block.rule").unwrap()])
                .unwrap(),
        ])
        .unwrap();
    let mut inner_ran = false;
    let outer_env = tx
        .elevated(&base, reason(), outer_scope, async |tx, outer| {
            let inner = tx
                .elevated(&outer, reason(), inner_scope, async |tx, inner| {
                    // the inner environment holds the inner scope only: base principal + inner
                    match inner.principal().elevation() {
                        Elevation::Scoped { scope, .. } => {
                            assert_eq!(**scope, expected_inner);
                        }
                        other => panic!("not scoped: {other:?}"),
                    }
                    assert_eq!(inner.principal().subject(), outer.principal().subject());
                    assert_eq!(inner.principal().actor(), outer.principal().actor());
                    // the outer environment is still valid inside the inner body
                    note.with_env(outer.clone()).value(tx, "name").await?;
                    tag.with_env(inner.clone()).value(tx, "name").await?;
                    Ok(inner)
                })
                .await?;
            assert_ne!(block_of(&inner), block_of(&outer));
            // the inner block closed: its environment is refused, the outer one works again
            let message = outside(block_of(&inner));
            assert_error(
                &tag.with_env(inner).value(tx, "name").await.unwrap_err(),
                ErrorKind::Programming,
                &message,
            );
            note.with_env(outer.clone()).value(tx, "name").await?;
            // a refused inner entry leaves the outer block open
            let error = tx
                .elevated(&outer, reason(), bad_inner, async |_tx, _env| {
                    inner_ran = true;
                    Ok(())
                })
                .await
                .unwrap_err();
            assert_error(&error, ErrorKind::Programming, LIFT_REFUSAL);
            note.with_env(outer.clone()).value(tx, "name").await?;

            // derivations keep the lifetime
            let off = outer.sudo(false);
            assert!(!off.su());
            assert_eq!(off.principal().elevation(), &Elevation::None);
            note.with_env(off.clone()).value(tx, "name").await?;
            let other = outer.with_user(52);
            assert_eq!(other.uid(), 52);
            let debug = outer
                .with_debug(true)
                .with_context([("lang", Value::str("en_US"))]);
            note.with_env(debug.clone()).value(tx, "name").await?;
            let company = outer.with_company(1)?;
            note.with_env(company.clone()).value(tx, "name").await?;
            // widening refuses every use, reported by the companies gate
            let message = widened(block_of(&outer));
            // the audited escape hatch refuses too, before it records anything
            assert_error(
                &tx.audited_unscoped_sudo(&outer, "prin_block").unwrap_err(),
                ErrorKind::Programming,
                &message,
            );
            let action = SuperuserAction {
                action_id: 1,
                root_action_id: 1,
                group_ids: Box::new([1]),
                flag_write_uid: None,
                flag_write_date_utc_micros: None,
            };
            assert_error(
                &tx.audited_superuser_env(&outer, &action).unwrap_err(),
                ErrorKind::Programming,
                &message,
            );
            for wide in [outer.sudo(true), outer.with_user(SUPERUSER_ID)] {
                assert!(!wide.su(), "a refused environment never acts as superuser");
                assert_error(
                    &wide.companies().unwrap_err(),
                    ErrorKind::Programming,
                    &message,
                );
                assert_error(
                    &note
                        .with_env(wide.clone())
                        .value(tx, "name")
                        .await
                        .unwrap_err(),
                    ErrorKind::Programming,
                    &message,
                );
                // and it stays refused through further derivations
                assert_error(
                    &wide.sudo(false).companies().unwrap_err(),
                    ErrorKind::Programming,
                    &message,
                );
            }
            Ok((outer, off, other, debug, company))
        })
        .await
        .unwrap();
    assert!(!inner_ran, "the refused inner body never ran");
    let note_model = tx.registry().model_by_id(model(&tx, NOTE)).name;
    let tag_model = tx.registry().model_by_id(model(&tx, TAG)).name;
    let (outer, off, other, debug, company) = outer_env;
    let message = outside(block_of(&outer));
    for derived in [outer, off, other, debug, company] {
        assert_error(
            &note
                .with_env(derived)
                .value(&mut tx, "name")
                .await
                .unwrap_err(),
            ErrorKind::Programming,
            &message,
        );
    }
    tx.commit().await.unwrap();
    // each block is its own observation; the refused entries (the bad inner scope, both audited
    // escape hatches) recorded nothing: still exactly the two block entries
    let evidence = events(&h).await;
    assert_eq!(evidence.len(), 2, "{evidence:?}");
    assert_eq!(evidence[0]["grant"]["items"][0]["model"], note_model);
    assert_eq!(evidence[1]["grant"]["items"][0]["model"], tag_model);
    assert_eq!(evidence[1]["original"]["subject"], 51);
    h.cleanup().await.unwrap();
}

/// id or a field id out of its range, taken from another registry), a lifted rule, and exhausted
/// block numbers and transaction serials; no body runs and nothing is skipped. Empty and duplicate
/// scopes cannot even be built. Ids of another registry that happen to be in range cannot be told
/// apart from this registry's (registry ids carry no registry identity).
#[tokio::test]
async fn invalid_scopes_and_exhausted_identities_are_refused_before_the_body() {
    let h = harness().await;
    let foreign = Harness::builder([&base_users::ADDON, &fixture::ADDON, &foreign::ADDON])
        .registry_only()
        .unwrap();
    let mut tx = begin_user(&h, 51).await;
    let base = tx.env();
    let mine = Arc::clone(tx.registry());
    let read = OperationSet::of([Operation::Read]);

    // empty and duplicate scopes are refused when built
    assert_error(
        &Error::from(ElevationScope::new([]).unwrap_err()),
        ErrorKind::Programming,
        "invalid elevation scope: a scope with no item opens nothing",
    );
    let note = model(&tx, NOTE);
    assert_error(
        &Error::from(
            ElevationScope::new([
                ScopeItem::new(note, read).unwrap(),
                ScopeItem::new(note, read).unwrap(),
            ])
            .unwrap_err(),
        ),
        ErrorKind::Programming,
        &format!(
            "invalid elevation scope: model {} has two items (merge them into one)",
            note.index()
        ),
    );

    let mut ran = false;
    // a model id beyond this registry
    let far = foreign.models().iter().map(|m| m.id).max().unwrap();
    assert!(
        far.index() >= mine.models().len(),
        "fixture: the id is foreign"
    );
    let error = tx
        .elevated(
            &base,
            reason(),
            ElevationScope::new([ScopeItem::new(far, read).unwrap()]).unwrap(),
            async |_tx, _env| {
                ran = true;
                Ok(())
            },
        )
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        &format!(
            "invalid elevation scope: model {} does not exist in this transaction's registry",
            far.index()
        ),
    );
    // a field id beyond the fields of a model of this registry
    let tag = mine.model(TAG).unwrap();
    let wide = foreign.model("prin.block.wide").unwrap();
    let field = wide.fields.iter().map(|f| f.id).max().unwrap();
    assert!(
        field.index() >= tag.fields.len(),
        "fixture: the field is foreign"
    );
    let error = tx
        .elevated(
            &base,
            reason(),
            ElevationScope::new([ScopeItem::new(tag.id, read)
                .unwrap()
                .fields(FieldAllow::named([field]).unwrap())])
            .unwrap(),
            async |_tx, _env| {
                ran = true;
                Ok(())
            },
        )
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        &format!(
            "invalid elevation scope: field {} does not exist in model '{TAG}' of this \
             transaction's registry",
            field.index()
        ),
    );
    // a lifted rule (the harness's `AllowAll` has no restriction rule to lift)
    let error = tx
        .elevated(
            &base,
            reason(),
            ElevationScope::new([ScopeItem::new(tag.id, read)
                .unwrap()
                .lift([QualifiedXmlId::parse("prin_block.rule").unwrap()])
                .unwrap()])
            .unwrap(),
            async |_tx, _env| {
                ran = true;
                Ok(())
            },
        )
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Programming, LIFT_REFUSAL);

    // exhausted block numbers: the last number is handed out once, then admission refuses
    tx.set_next_block_seq_for_tests(u32::MAX);
    let last = tx
        .elevated(&base, reason(), read_write(&tx), async |_tx, env| {
            Ok(block_of(&env))
        })
        .await
        .unwrap();
    assert_eq!(last.seq(), u32::MAX);
    let error = tx
        .elevated(&base, reason(), read_write(&tx), async |_tx, _env| {
            ran = true;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        &format!(
            "elevation block identity exhausted: transaction {} handed out every block number \
             (block numbers are never reused)",
            last.tx().get()
        ),
    );
    assert!(!ran, "no refused body ran");
    tx.commit().await.unwrap();

    // an exhausted transaction serial (the seam makes this transaction's next serial come from
    // a counter at its maximum; the process counter is untouched)
    let mut tx = begin_user(&h, 51).await;
    let base = tx.env();
    tx.exhaust_tx_serial_for_tests();
    let error = tx
        .elevated(&base, reason(), read_write(&tx), async |_tx, _env| {
            ran = true;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        "elevation block identity exhausted: this process handed out every transaction serial \
         (serials are never reused)",
    );
    assert!(!ran, "no refused body ran");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The admission-failure half: with an audit store that is not ready, entry
/// fails with AUDIT's error before any statement and the body never runs.
async fn store_not_ready_refuses_entry_before_the_body() {
    let h = Harness::builder([&base_users::ADDON, &fixture::ADDON])
        .without_audit_store()
        .build()
        .await
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::for_principal(PrincipalBuilder::user(user(51)).build().unwrap())
                .operation_identity(identity()),
        )
        .await
        .unwrap();
    let base = tx.env();
    let before = tx.query_count();
    let mut ran = false;
    let error = tx
        .elevated(&base, reason(), read_write(&tx), async |_tx, _env| {
            ran = true;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Internal,
        "a later release (unavailable): the audit store is not ready (the store was not checked yet); \
         nothing was recorded, fix the store (run `erp audit maintain` or the runtime grant \
         publication as the owner role)",
    );
    assert!(!ran, "the body never ran");
    assert_eq!(tx.query_count(), before, "no statement");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// entry; the committed row records reason, scope, subject, actor and correlation; a savepoint
/// rolled back keeps the evidence, marked; an INSERT that fails at commit refuses the commit and
/// the business change is absent; with an audit store that is not ready, entry fails before the
/// body (observation precedes the body).
#[tokio::test]
async fn elevation_evidence_is_deduplicated_and_committed() {
    let h = harness().await;
    let operation = identity();
    let principal = PrincipalBuilder::user(user(51)).build().unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal).operation_identity(operation.clone()))
        .await
        .unwrap();
    let base = tx.env();
    let mut first: Option<BlockId> = None;
    for _ in 0..2 {
        let block = tx
            .elevated(&base, reason(), read_write(&tx), async |_tx, env| {
                Ok(block_of(&env))
            })
            .await
            .unwrap();
        first.get_or_insert(block);
    }
    let impersonated = base.with_user(52);
    tx.elevated(
        &impersonated,
        reason(),
        read_write(&tx),
        async |_tx, _env| Ok(()),
    )
    .await
    .unwrap();
    // under a savepoint that rolls back
    let rolled: erp_fields::Result<()> = tx
        .savepoint(async |tx| {
            let s = scope(tx, TAG, &[Operation::Read]);
            tx.elevated(&base, reason(), s, async |_tx, _env| Ok(()))
                .await?;
            Err(Error::validation("undo"))
        })
        .await;
    assert_error(&rolled.unwrap_err(), ErrorKind::Validation, "undo");
    let note_model = tx.registry().model_by_id(model(&tx, NOTE)).name;
    let tag_model = tx.registry().model_by_id(model(&tx, TAG)).name;
    tx.commit().await.unwrap();

    let events = events(&h).await;
    assert_eq!(events.len(), 3, "{events:?}");
    let own = &events[0];
    assert_eq!(own["type"], "elevation");
    assert_eq!(own["reason"], REASON);
    assert_eq!(own["count"], 2);
    assert_eq!(own["state"], "effective");
    assert_eq!(own["grant"]["kind"], "scoped");
    assert_eq!(own["grant"]["block"], first.unwrap().seq());
    assert_eq!(own["grant"]["items"][0]["model"], note_model);
    assert_eq!(
        own["grant"]["items"][0]["ops"],
        serde_json::json!(["read", "write"])
    );
    assert_eq!(own["grant"]["items"][0]["fields"], "*");
    assert_eq!(own["original"]["subject"], 51);
    assert_eq!(own["original"]["actor_kind"], "user");
    assert_eq!(own["original"]["actor_id"], 51);
    assert_eq!(own["effective_subject"], 51);
    let other = &events[1];
    assert_eq!(other["count"], 1);
    assert_eq!(other["original"]["subject"], 52);
    assert_eq!(other["original"]["actor_id"], 51);
    assert_eq!(other["effective_subject"], 52);
    let rolled_back = &events[2];
    assert_eq!(rolled_back["grant"]["items"][0]["model"], tag_model);
    assert_eq!(rolled_back["state"], "savepoint_rolled_back");
    assert_eq!(rolled_back["rolled_back"], 1);
    let row = owner(&h)
        .await
        .query_one(
            "SELECT correlation_id::text, operation_id::text, subject_id FROM \
             erp_event.ir_audit_entry",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, String>(0),
        operation.correlation().as_str().to_owned()
    );
    assert_eq!(
        row.get::<_, String>(1),
        operation.operation().as_str().to_owned()
    );
    assert_eq!(row.get::<_, i64>(2), 51);

    // an INSERT that fails at commit refuses the commit and takes the business change with it
    let today: String = owner(&h)
        .await
        .query_one(
            "SELECT 'ir_audit_entry_p' || to_char((now() AT TIME ZONE 'UTC')::date, 'YYYYMMDD')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    owner(&h)
        .await
        .batch_execute(&format!("DROP TABLE erp_event.{today}"))
        .await
        .unwrap();
    let mut tx = begin_user(&h, 51).await;
    let base = tx.env();
    let s = read_write(&tx);
    tx.elevated(&base, reason(), s, async |tx, env| {
        env.records(NOTE)?
            .create_values(tx, &[vals(&[("name", Value::str("lost"))])])
            .await?;
        Ok(())
    })
    .await
    .unwrap();
    let error = tx.commit().await.expect_err("no partition for today");
    assert_error(
        &error,
        ErrorKind::Internal,
        "a later release (insertion): the audit INSERT failed (SQLSTATE 23514); no partition of \
         erp_event.ir_audit_entry covers the transaction's day; the event partition upkeep has not \
         run (it runs at server start and hourly; `erp audit maintain` refreshes both event tables)",
    );
    let lost: i64 = owner(&h)
        .await
        .query_one(
            "SELECT count(*) FROM prin_block_note WHERE name = 'lost'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(lost, 0, "the business change rolled back");
    h.cleanup().await.unwrap();
    store_not_ready_refuses_entry_before_the_body().await;
}

/// answer are in rpc_audit_policy.rs) a physical read-only transaction without a read-audit owner
/// refuses scoped entry and the declared unscoped exception before the body and before any
/// statement; one opened with an owner admits both, stays `READ ONLY` and leaves the elevation
/// evidence in its attempt; a read-only API key in a writable transaction opens a block and reads
/// in it, its business write in the block is still refused with the read-only key error, and the
/// block leaves real elevation evidence.
#[tokio::test]
async fn read_only_transactions_refuse_and_read_only_keys_stay_read_only() {
    let h = harness().await;
    // seed a note to read
    let mut setup = begin_user(&h, 51).await;
    seed_note(&mut setup).await;
    setup.commit().await.unwrap();

    // without an owner: refused before the body, before any statement, nothing recorded
    let mut tx = begin_read_only(&h, false).await;
    assert!(tx.read_audit_owner().is_none());
    let base = tx.env();
    let before = tx.query_count();
    let mut ran = false;
    let error = tx
        .elevated(&base, reason(), read_write(&tx), async |_tx, _env| {
            ran = true;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Programming, READ_ONLY_REFUSAL);
    assert!(!ran, "the body never ran");
    let error = tx.audited_unscoped_sudo(&base, "prin_block").unwrap_err();
    assert_error(&error, ErrorKind::Programming, READ_ONLY_REFUSAL);
    assert_eq!(tx.query_count(), before, "no statement");
    assert!(!tx.take_audit_attempt().has_elevation(), "nothing recorded");
    drop(tx);

    // with an owner: the block opens, reads, the transaction stays READ ONLY, the evidence is
    // buffered for the owner's attempt row
    let mut tx = begin_read_only(&h, true).await;
    let owner = tx.read_audit_owner().expect("the declared owner").name();
    assert_eq!(owner, OWNER);
    assert!(tx.intent().is_read());
    let base = tx.env();
    let s = read_write(&tx);
    let (name, read_only) = tx
        .elevated(&base, reason(), s, async |tx, env| {
            let notes = env.records(NOTE)?.search(tx, &Domain::TRUE).await?;
            let name = notes.value(tx, "name").await?;
            let state = tx
                .query("SELECT current_setting('transaction_read_only')", &[])
                .await?;
            Ok((name, state[0].get::<_, String>(0)))
        })
        .await
        .unwrap();
    assert_eq!(name, Value::str("n1"));
    assert_eq!(read_only, "on");
    let evidence = tx.take_audit_attempt();
    assert!(evidence.has_elevation(), "{evidence:?}");
    let shown = format!("{evidence:?}");
    assert!(shown.contains(REASON), "{shown}");
    assert!(shown.contains("Scoped"), "{shown}");
    assert!(shown.contains("subject: 51"), "{shown}");
    drop(tx);

    // the declared unscoped exception: same two halves (the read half needs an owner)
    let mut tx = begin_read_only(&h, true).await;
    let base = tx.env();
    let widened = tx.audited_unscoped_sudo(&base, "prin_block").unwrap();
    assert!(widened.principal().elevation().is_full());
    let state = tx
        .query("SELECT current_setting('transaction_read_only')", &[])
        .await
        .unwrap();
    assert_eq!(state[0].get::<_, String>(0), "on");
    let evidence = tx.take_audit_attempt();
    assert!(evidence.has_elevation(), "{evidence:?}");
    let shown = format!("{evidence:?}");
    assert!(shown.contains("core.sudo.unscoped"), "{shown}");
    drop(tx);

    // the superuser server action keeps the write-only rule, owner or not
    let mut tx = begin_read_only(&h, true).await;
    let base = tx.env();
    let action = SuperuserAction {
        action_id: 1,
        root_action_id: 1,
        group_ids: Box::new([1]),
        flag_write_uid: None,
        flag_write_date_utc_micros: None,
    };
    let error = tx.audited_superuser_env(&base, &action).unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        "a later release (read-only-transaction): lifecycle changes, audited superuser runs and call \
         summaries need a write transaction",
    );
    assert!(!tx.take_audit_attempt().has_elevation());
    drop(tx);

    let key = Scope::builder(ApiKeyId::new(7).unwrap(), "read only")
        .models(ModelAllow::All)
        .methods(MethodAllow::All)
        .read_only(true)
        .build()
        .unwrap();
    let principal = PrincipalBuilder::user(user(51)).scope(key).build().unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::for_principal(principal).operation_identity(identity()))
        .await
        .unwrap();
    assert!(!tx.intent().is_read(), "a writable transaction");
    let base = tx.env();
    let s = read_write(&tx);
    let refused = tx
        .elevated(&base, reason(), s, async |tx, env| {
            let notes = env.records(NOTE)?.search(tx, &Domain::TRUE).await?;
            assert_eq!(notes.value(tx, "name").await?, Value::str("n1"));
            Ok(notes
                .write_values(tx, &vals(&[("name", Value::str("changed"))]))
                .await
                .unwrap_err())
        })
        .await
        .unwrap();
    assert_error(
        &refused,
        ErrorKind::Access,
        "The API key 'read only' is read-only: it may not write 'Block Note' (prin.block.note).",
    );
    tx.commit().await.unwrap();
    let evidence = events(&h).await;
    assert_eq!(evidence.len(), 1, "{evidence:?}");
    assert_eq!(evidence[0]["reason"], REASON);
    assert_eq!(evidence[0]["original"]["actor_kind"], "api_key_user");
    assert_eq!(evidence[0]["original"]["actor_id"], 7);
    h.cleanup().await.unwrap();
}

/// transaction and may roll it back or commit it; the evidence of an owned read-only transaction
/// is still handed to the owner's attempt row afterwards, and a later elevation adds to it.
#[tokio::test]
async fn an_owned_read_keeps_its_elevation_evidence_when_the_body_ends_the_transaction() {
    let h = harness().await;

    // elevate, roll back inside the body, then the service commits
    let mut tx = begin_read_only(&h, true).await;
    let base = tx.env();
    tx.audited_unscoped_sudo(&base, "prin_block").unwrap();
    tx.rollback().await.unwrap();
    tx.commit().await.unwrap();
    let evidence = tx.take_audit_attempt();
    assert!(evidence.has_elevation(), "{evidence:?}");
    assert!(
        format!("{evidence:?}").contains("core.sudo.unscoped"),
        "{evidence:?}"
    );
    drop(tx);

    // the same with the pipelined commit
    let mut tx = begin_read_only(&h, true).await;
    let base = tx.env();
    tx.audited_unscoped_sudo(&base, "prin_block").unwrap();
    tx.rollback().await.unwrap();
    tx.commit_pipelined(None).await.unwrap();
    assert!(tx.take_audit_attempt().has_elevation());
    drop(tx);

    // elevate, roll back, elevate again for another reason: both are evidence
    let mut tx = begin_read_only(&h, true).await;
    let base = tx.env();
    tx.audited_unscoped_sudo(&base, "prin_block").unwrap();
    tx.rollback().await.unwrap();
    let base = tx.env();
    tx.elevated(&base, reason(), read_write(&tx), async |_tx, _env| Ok(()))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let shown = format!("{:?}", tx.take_audit_attempt());
    assert!(shown.contains("core.sudo.unscoped"), "{shown}");
    assert!(shown.contains(REASON), "{shown}");
    drop(tx);

    // the same elevation before and after a rollback in the body is one entry with its count
    // (the stash is merged before the dedupe search)
    let mut tx = begin_read_only(&h, true).await;
    let base = tx.env();
    tx.audited_unscoped_sudo(&base, "prin_block").unwrap();
    tx.rollback().await.unwrap();
    let base = tx.env();
    tx.audited_unscoped_sudo(&base, "prin_block").unwrap();
    tx.commit().await.unwrap();
    let shown = format!("{:?}", tx.take_audit_attempt());
    assert_eq!(shown.matches("core.sudo.unscoped").count(), 1, "{shown}");
    assert!(shown.contains("count: 2"), "{shown}");
    drop(tx);

    // a write transaction keeps the old behaviour: a rollback clears (an unowned read cannot
    // elevate at all, see the refusal tests)
    let mut tx = begin_user(&h, 51).await;
    let base = tx.env();
    tx.audited_unscoped_sudo(&base, "prin_block").unwrap();
    tx.rollback().await.unwrap();
    tx.commit().await.unwrap();
    assert!(!tx.take_audit_attempt().has_elevation());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// programming refusal for the missing owner even when the audit store is not ready, and an owned
/// one on a store that was never checked is refused with AUDIT's `unavailable` error.
#[tokio::test]
async fn read_only_refusals_do_not_depend_on_the_store_and_an_unchecked_store_refuses_owned_reads()
{
    let h = Harness::builder([&base_users::ADDON, &fixture::ADDON])
        .without_audit_store()
        .build()
        .await
        .unwrap();
    let mut tx = begin_read_only(&h, false).await;
    let base = tx.env();
    let error = tx.audited_unscoped_sudo(&base, "prin_block").unwrap_err();
    assert_error(&error, ErrorKind::Programming, READ_ONLY_REFUSAL);
    drop(tx);

    let unchecked = "a later release (unavailable): the audit store is not ready (the store was not \
                     checked yet); nothing was recorded, fix the store (run `erp audit maintain` \
                     or the runtime grant publication as the owner role)";
    let mut tx = begin_read_only(&h, true).await;
    let base = tx.env();
    let error = tx.audited_unscoped_sudo(&base, "prin_block").unwrap_err();
    assert_error(&error, ErrorKind::Internal, unchecked);
    let mut ran = false;
    let error = tx
        .elevated(&base, reason(), read_write(&tx), async |_tx, _env| {
            ran = true;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Internal, unchecked);
    assert!(!ran, "the body never ran");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// owner declared before; an owner and a read intent are declared together.
#[test]
fn the_read_audit_owner_declaration() {
    let error = ReadAuditOwner::new("").unwrap_err();
    assert_error(
        &error,
        ErrorKind::Programming,
        "a read-audit owner needs a name",
    );
    let principal = PrincipalBuilder::user(user(51)).build().unwrap();
    let options = BeginOptions::for_principal(principal.clone());
    assert!(options.read_audit_owner().is_none());
    let owned = options
        .clone()
        .read_only_audited("a.b.c", ReadAuditOwner::new(OWNER).unwrap());
    assert!(owned.intent().is_read());
    assert_eq!(
        owned.read_audit_owner().map(ReadAuditOwner::name),
        Some(OWNER)
    );
    let cleared = owned.read_only("a.b.c");
    assert!(cleared.intent().is_read());
    assert!(cleared.read_audit_owner().is_none());
    let credential =
        CredentialOptions::new().read_only_audited("a.b.c", ReadAuditOwner::new(OWNER).unwrap());
    assert_eq!(
        credential.read_audit_owner().map(ReadAuditOwner::name),
        Some(OWNER)
    );
    assert!(CredentialOptions::new().read_audit_owner().is_none());
}

async fn property_harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &property::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let currency = erp_testkit::fixtures::seed_currency_in(&mut tx)
        .await
        .unwrap();
    tx.execute(
        "INSERT INTO res_company (id, name, currency_id) VALUES (1, 'One', $1) \
         ON CONFLICT DO NOTHING",
        &[&currency],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO res_users (id, login, name, active, share) VALUES \
         (51, 'u51', 'u51', true, false) ON CONFLICT DO NOTHING",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

/// The writer of the A5 tests: user 51 working in company 1, reached from the System transaction
/// (as the company-dependent runtime tests reach their users), and a property record to write.
async fn property_setup(tx: &mut Transaction) -> (Env, Records) {
    let system = tx
        .env()
        .with_context([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    system
        .records("decimal.precision")
        .unwrap()
        .create_values(
            tx,
            &[vals(&[
                ("name", Value::str("Prin Block Precision")),
                ("digits", Value::Int(4)),
            ])],
        )
        .await
        .unwrap();
    let rec = system
        .records("prin.block.property")
        .unwrap()
        .create_values(tx, &[vals(&[("name", Value::str("p"))])])
        .await
        .unwrap();
    // the precision is resolved once, by the System writer (base's precision lookup elevates with
    // the public `sudo(true)`, which a block environment refuses)
    rec.write_values(
        tx,
        &vals(&[("named", Value::Decimal("0.0001".parse().unwrap()))]),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    (system.with_user(51), rec)
}

/// The stored company map of `named`, read with a raw statement (which flushes nothing).
async fn stored_named(tx: &mut Transaction, rec: &Records) -> serde_json::Value {
    tx.query(
        "SELECT named FROM prin_block_property WHERE id = $1",
        &[&rec.ids()[0]],
    )
    .await
    .unwrap()[0]
        .get(0)
}

async fn begin_system(h: &Harness) -> Transaction {
    h.orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).operation_identity(identity()))
        .await
        .unwrap()
}

/// A5 (success exit) a company-dependent write made in a block is flushed before the
/// block closes: the later commit raises no lifetime error and the value is stored.
#[tokio::test]
async fn a5_success_exit_flushes_inside_the_block() {
    let h = property_harness().await;
    let mut tx = begin_system(&h).await;
    let (writer, rec) = property_setup(&mut tx).await;
    let property = model(&tx, "prin.block.property");
    let s = ElevationScope::new([
        ScopeItem::new(property, OperationSet::of([Operation::Write])).unwrap(),
    ])
    .unwrap();
    tx.elevated(&writer, reason(), s, async |tx, env| {
        rec.with_env(env)
            .write_values(
                tx,
                &vals(&[("named", Value::Decimal("1.2345".parse().unwrap()))]),
            )
            .await
    })
    .await
    .unwrap();
    // flushed before the block closed: in the database before any later flush
    assert_eq!(
        stored_named(&mut tx, &rec).await,
        serde_json::json!({"1": "1.2345"})
    );
    tx.commit().await.unwrap();
    let stored: serde_json::Value = owner(&h)
        .await
        .query_one(
            "SELECT named FROM prin_block_property WHERE id = $1",
            &[&rec.ids()[0]],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(stored, serde_json::json!({"1": "1.2345"}));
    h.cleanup().await.unwrap();
}

/// A5 (Err exit) a company-dependent write left pending by a body that returns `Err` is
/// flushed at block exit, under the still-open block: the body's error stays the block's error, and
/// the later flush and commit neither raise the lifetime error nor fail otherwise.
#[tokio::test]
async fn a5_err_exit_flushes_inside_the_block() {
    let h = property_harness().await;
    let mut tx = begin_system(&h).await;
    let (writer, rec) = property_setup(&mut tx).await;
    let property = model(&tx, "prin.block.property");
    let s = ElevationScope::new([
        ScopeItem::new(property, OperationSet::of([Operation::Write])).unwrap(),
    ])
    .unwrap();
    let error = tx
        .elevated(&writer, reason(), s, async |tx, env| {
            rec.with_env(env)
                .write_values(
                    tx,
                    &vals(&[("named", Value::Decimal("2.1234".parse().unwrap()))]),
                )
                .await?;
            Err::<(), _>(Error::validation("the body refused after writing"))
        })
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Validation,
        "the body refused after writing",
    );
    // flushed at block exit, under the open block: in the database before any later flush
    assert_eq!(
        stored_named(&mut tx, &rec).await,
        serde_json::json!({"1": "2.1234"})
    );
    tx.flush().await.unwrap();
    tx.commit().await.unwrap();
    let stored: serde_json::Value = owner(&h)
        .await
        .query_one(
            "SELECT named FROM prin_block_property WHERE id = $1",
            &[&rec.ids()[0]],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(stored, serde_json::json!({"1": "2.1234"}));
    h.cleanup().await.unwrap();
}

/// The error of a failing flush outside any block, from our own code: a pending write of a negative
/// size (refused by the table's CHECK constraint when the flush sends it).
async fn reference_flush_error(h: &Harness) -> Error {
    let mut tx = begin_user(h, 51).await;
    let (_, note) = seed_note(&mut tx).await;
    note.write_values(&mut tx, &vals(&[("size", Value::Int(-1))]))
        .await
        .unwrap();
    let error = tx.flush().await.unwrap_err();
    drop(tx);
    error
}

/// A5 (exit flush failures) when the exit flush fails after an `Ok` body, the flush's
/// error is the block's error; after an `Err` body, one error carries both messages and keeps the
/// body's kind and SQLSTATE.
#[tokio::test]
async fn a5_exit_flush_failures_are_reported() {
    let h = harness().await;
    let reference = reference_flush_error(&h).await;

    // Ok body
    let mut tx = begin_user(&h, 51).await;
    let (_, note) = seed_note(&mut tx).await;
    let base = tx.env();
    let s = read_write(&tx);
    let error = tx
        .elevated(&base, reason(), s, async |tx, env| {
            note.with_env(env)
                .write_values(tx, &vals(&[("size", Value::Int(-1))]))
                .await
        })
        .await
        .unwrap_err();
    assert_error(&error, reference.kind(), reference.message());
    drop(tx);

    // Err body
    let mut tx = begin_user(&h, 51).await;
    let (_, note) = seed_note(&mut tx).await;
    let base = tx.env();
    let s = read_write(&tx);
    let error = tx
        .elevated(&base, reason(), s, async |tx, env| {
            note.with_env(env)
                .write_values(tx, &vals(&[("size", Value::Int(-1))]))
                .await?;
            Err::<(), _>(
                Error::new(ErrorKind::Validation, "the body refused")
                    .with_sqlstate(Some("P0001".to_owned())),
            )
        })
        .await
        .unwrap_err();
    assert_error(
        &error,
        ErrorKind::Validation,
        &format!(
            "the body refused (and the block's exit flush failed: {})",
            reference.message()
        ),
    );
    assert_eq!(
        error.sqlstate(),
        Some("P0001"),
        "the body's SQLSTATE is kept"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The transaction's ORM cannot be switched while a block is open: the block's scope was
/// validated against the current registry. The refusal names the block.
#[tokio::test]
async fn switch_orm_is_refused_inside_a_block() {
    let h = harness().await;
    let other = h
        .orm()
        .rebuild(Arc::new(
            erp_registry::RegistryBuilder::new([&base_users::ADDON, &fixture::ADDON])
                .build()
                .unwrap(),
        ))
        .build()
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).operation_identity(identity()))
        .await
        .unwrap();
    let base = tx.env();
    let s = read_write(&tx);
    let (block, refused) = tx
        .elevated(&base, reason(), s, async |tx, env| {
            Ok((block_of(&env), tx.switch_orm(&other).await))
        })
        .await
        .unwrap();
    assert_error(
        &refused.unwrap_err(),
        ErrorKind::Programming,
        &format!(
            "cannot switch the transaction's ORM inside elevation block {}#{}",
            block.tx().get(),
            block.seq()
        ),
    );
    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Job administration and cancellation by key refuse an environment of a closed block before
/// anything else, also a System-based one, which the System shortcut of job administration would
/// otherwise let through without asking the engine.
#[tokio::test]
async fn escaped_environments_administer_no_job() {
    let h = harness().await;
    let key = JobKey::new("prin-block").unwrap();
    for system in [true, false] {
        let mut tx = if system {
            begin_system(&h).await
        } else {
            begin_user(&h, 51).await
        };
        let base = tx.env();
        let s = read_write(&tx);
        let escaped = tx
            .elevated(&base, reason(), s, async |_tx, env| Ok(env))
            .await
            .unwrap();
        assert_eq!(
            escaped.principal().kind() == PrincipalKind::System,
            system,
            "fixture: the escaped environment's kind"
        );
        let message = outside(block_of(&escaped));
        assert_error(
            &tx.job_hold(&escaped, JobRef::new(1, erp_fields::Uuid::NIL))
                .await
                .unwrap_err(),
            ErrorKind::Programming,
            &message,
        );
        assert_error(
            &tx.job_cancel_by_keys(&escaped, "prin_block.kind", std::slice::from_ref(&key))
                .await
                .unwrap_err(),
            ErrorKind::Programming,
            &message,
        );
        tx.commit().await.unwrap();
    }
    h.cleanup().await.unwrap();
}

const ROLL_BACK_FIRST: &str =
    "cannot open an elevation block: the transaction is aborted or released; roll back";

/// A poisoned, an aborted (after a failed statement) and a released transaction open no block:
/// entry is refused before the body and before any evidence is recorded.
#[tokio::test]
async fn transactions_that_must_roll_back_open_no_block() {
    let h = harness().await;
    let mut ran = false;

    // poisoned: refused with the poison's own error
    let mut tx = begin_user(&h, 51).await;
    let base = tx.env();
    tx.poison_for_tests(&Error::validation("poisoned for the test"));
    let error = tx
        .elevated(&base, reason(), read_write(&tx), async |_tx, _env| {
            ran = true;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Validation, "poisoned for the test");
    assert!(!tx.take_audit_attempt().has_elevation(), "no evidence");
    drop(tx);

    // aborted by a failed statement
    let mut tx = begin_user(&h, 51).await;
    let base = tx.env();
    tx.query("SELECT 1/0", &[]).await.unwrap_err();
    let error = tx
        .elevated(&base, reason(), read_write(&tx), async |_tx, _env| {
            ran = true;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Programming, ROLL_BACK_FIRST);
    assert!(!tx.take_audit_attempt().has_elevation(), "no evidence");
    drop(tx);

    // released: the connection went away before COMMIT
    let mut tx = begin_user(&h, 51).await;
    let base = tx.env();
    tx.query("SELECT 1", &[]).await.unwrap();
    tx.inject_commit_fault(CommitFault::ConnectionClosedBeforeCommit);
    assert!(matches!(
        tx.commit_pipelined(None).await,
        Err(CommitError::Unknown(_))
    ));
    assert!(tx.is_released(), "fixture: the transaction is released");
    let error = tx
        .elevated(&base, reason(), read_write(&tx), async |_tx, _env| {
            ran = true;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_error(&error, ErrorKind::Programming, ROLL_BACK_FIRST);
    assert!(!tx.take_audit_attempt().has_elevation(), "no evidence");
    drop(tx);

    assert!(!ran, "no refused body ran");
    h.cleanup().await.unwrap();
}
