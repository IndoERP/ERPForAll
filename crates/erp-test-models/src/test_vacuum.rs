//! Fixture models for the transient vacuum engine (package B).
//!
//! Every transient model shares one `unlink` override that makes the cleanup observable: it
//! writes one `test_vac.log` row per deleted record (in the deleting transaction, so a rollback
//! takes the log with it), fails when a deleted record is named `boom`, and, when one is named
//! `steal`, moves the model's vacuum state row to a newer epoch (as a second process taking the
//! lease over would) before the batch updates it.
//!
//! The process tests (package C) add three more names, each making the batch of the process that
//! deletes the record wait on advisory locks, which are visible across transactions and processes:
//! `park` takes [`PARK_READY_KEY`] (so a test sees the batch is inside the hook) and then waits
//! until no other session holds [`PARK_GATE_KEY`]; `lockwait` waits for the gate lock with a
//! blocking request (the batch's lock timeout ends it); `hang` sleeps in the database (the
//! statement timeout ends it).

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_vacuum");

/// Advisory lock a `park` hook holds while it is inside its batch.
pub const PARK_READY_KEY: i64 = 7_300_001;
/// Advisory lock a test holds to keep `park` and `lockwait` hooks waiting.
pub const PARK_GATE_KEY: i64 = 7_300_002;
/// Polls (20 ms apart, in the database) of the `park` gate before the hook gives up: well under
/// the 30 s transaction timeout, which stays the real limit of a batch that stops polling.
const PARK_POLLS: u32 = 500;

/// What the shared hook does for the records being deleted from `model` (table `table`).
async fn observe(ids: &[i64], tx: &mut Transaction, model: &str, table: &str) -> Result<()> {
    if !tx.env().is_superuser() {
        return Err(erp_sdk::Error::user(
            "the vacuum must delete through the system principal",
        ));
    }
    let names = tx
        .query(
            &format!("SELECT name FROM {table} WHERE id = ANY($1)"),
            &[&ids.to_vec()],
        )
        .await?;
    let named = |wanted: &str| {
        names
            .iter()
            .any(|r| r.get::<_, Option<String>>(0).as_deref() == Some(wanted))
    };
    tx.execute(
        "INSERT INTO test_vac_log (entry) SELECT $1::text || ':' || id::text FROM unnest($2::int8[]) AS id",
        &[&model, &ids.to_vec()],
    )
    .await?;
    if named("steal") {
        tx.execute(
            "UPDATE erp_transient_vacuum_state SET epoch = epoch + 1 WHERE model = $1",
            &[&model],
        )
        .await?;
    }
    if named("drift") {
        tx.execute(
            "UPDATE erp_transient_vacuum_state SET policy_fingerprint = 'drifted' WHERE model = $1",
            &[&model],
        )
        .await?;
    }
    if named("park") {
        // blocking: a ready lock that is not taken fails the batch (lock timeout), never silently
        tx.query("SELECT pg_advisory_xact_lock($1)", &[&PARK_READY_KEY])
            .await?;
        let mut open = false;
        for _ in 0..PARK_POLLS {
            let rows = tx
                .query("SELECT pg_try_advisory_xact_lock($1)", &[&PARK_GATE_KEY])
                .await?;
            if rows.first().is_some_and(|r| r.get::<_, bool>(0)) {
                open = true;
                break;
            }
            tx.query("SELECT pg_sleep(0.02)", &[]).await?;
        }
        if !open {
            return Err(erp_sdk::Error::user("the park gate never opened"));
        }
    }
    if named("lockwait") {
        tx.query("SELECT pg_advisory_xact_lock($1)", &[&PARK_GATE_KEY])
            .await?;
    }
    if named("hang") {
        tx.query("SELECT pg_sleep(120)", &[]).await?;
    }
    if named("boom") {
        return Err(erp_sdk::Error::user("the unlink hook refused the record"));
    }
    Ok(())
}

/// Transient model with the default policy (one hour, no count limit).
#[model(
    name = "test_vac.default",
    description = "Vacuum defaults",
    addon = "test_vacuum",
    kind = "transient"
)]
pub struct Dflt {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(Dflt, addon = "test_vacuum")]
impl Dflt {
    /// Logs the deletion.
    #[method(override)]
    async fn unlink(this: &RecordSet<Dflt>, tx: &mut Transaction, sup: Super) -> Result<()> {
        observe(
            &this.as_records().ids(),
            tx,
            "test_vac.default",
            "test_vac_default",
        )
        .await?;
        sup.call(tx, this.as_records(), ()).await
    }
}

/// Transient model with the default age and a count limit of one.
#[model(
    name = "test_vac.counted",
    description = "Vacuum count one",
    addon = "test_vacuum",
    kind = "transient",
    transient_max_count = 1
)]
pub struct Counted {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(Counted, addon = "test_vacuum")]
impl Counted {
    /// Logs the deletion.
    #[method(override)]
    async fn unlink(this: &RecordSet<Counted>, tx: &mut Transaction, sup: Super) -> Result<()> {
        observe(
            &this.as_records().ids(),
            tx,
            "test_vac.counted",
            "test_vac_counted",
        )
        .await?;
        sup.call(tx, this.as_records(), ()).await
    }
}

/// Transient model with the age threshold disabled and a count limit of three.
#[model(
    name = "test_vac.latch",
    description = "Vacuum count only",
    addon = "test_vacuum",
    kind = "transient",
    transient_max_age_seconds = 0,
    transient_max_count = 3
)]
pub struct Latch {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(Latch, addon = "test_vacuum")]
impl Latch {
    /// Logs the deletion.
    #[method(override)]
    async fn unlink(this: &RecordSet<Latch>, tx: &mut Transaction, sup: Super) -> Result<()> {
        observe(
            &this.as_records().ids(),
            tx,
            "test_vac.latch",
            "test_vac_latch",
        )
        .await?;
        sup.call(tx, this.as_records(), ()).await
    }
}

/// Transient model with both thresholds disabled.
#[model(
    name = "test_vac.off",
    description = "Vacuum disabled",
    addon = "test_vacuum",
    kind = "transient",
    transient_max_age_seconds = 0,
    transient_max_count = 0
)]
pub struct Off {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// Regular model: the log of the hooks above, and a model the vacuum must refuse.
#[model(
    name = "test_vac.log",
    description = "Vacuum log",
    addon = "test_vacuum"
)]
pub struct Log {
    #[field(char)]
    pub entry: Option<Arc<str>>,
}

/// Regular model without a cleanup policy.
#[model(
    name = "test_vac.plain",
    description = "Not transient",
    addon = "test_vacuum"
)]
pub struct Plain {
    #[field(char)]
    pub name: Option<Arc<str>>,
}
