//! `test_rpc_ext` overrides some of them (hide, widen, inherit); `test_rpc_narrow`
//! narrows an effect, which must fail the ORM build.

use std::sync::atomic::{AtomicUsize, Ordering};

use erp_sdk::prelude::*;
use erp_sdk::rpc::Binary;
use erp_sdk::{Condition, ErrorKind, ValueMap};

erp_sdk::addon!(name = "test_rpc", depends = ["test_orm"]);

/// Attempts of the retry probes (`always_conflict`, `conflict_once`, `slow_increment`,
/// `locked`, `superseded_once`): a test reads it after the call.
pub static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
/// Reads made by `slow_increment`: its first two callers wait for each other (a test resets it).
pub static READS: AtomicUsize = AtomicUsize::new(0);
/// After-commit hooks run by `conflict_once`.
pub static HOOKS: AtomicUsize = AtomicUsize::new(0);

/// `test_rpc.model_a`, with a unique name ('s integrity error).
#[model(
    name = "test_rpc.model_a",
    description = "Model A",
    addon = "test_rpc",
    constraints = [("_name_uniq", "unique (name)", "The name must be unique")]
)]
pub struct ModelA {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_rpc.model_b",
        required,
        string = "required field"
    )]
    pub field_b1: Option<RecId>,
    #[field(
        many2one,
        comodel = "test_rpc.model_b",
        ondelete = "restrict",
        string = "restricted field"
    )]
    pub field_b2: Option<RecId>,
}

#[methods(ModelA, addon = "test_rpc")]
impl ModelA {
    /// Not exposed (private): 403 over RPC.
    #[method]
    async fn private_method(_this: &RecordSet<ModelA>, _tx: &mut Transaction) -> Result<String> {
        Ok("private".into())
    }
}

/// `test_http.stargate`.
#[model(
    name = "test_http.stargate",
    description = "Stargate",
    addon = "test_rpc"
)]
pub struct Stargate {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `test_http.galaxy`.
#[model(name = "test_http.galaxy", description = "Galaxy", addon = "test_rpc")]
pub struct Galaxy {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// Probes for effects, the read marker, retries and idempotency.
#[model(
    name = "test_rpc.effects",
    description = "RPC Effects",
    addon = "test_rpc"
)]
pub struct Effects {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub counter: i32,
    #[field(integer)]
    pub company: i32,
    /// A credential: the audit digest never covers its value (redaction v2).
    #[field(char, secret = true)]
    pub token: Option<Arc<str>>,
}

/// The database's view: `transaction_read_only` of the current transaction.
async fn read_only(tx: &mut Transaction) -> Result<bool> {
    let rows = tx
        .query("SELECT current_setting('transaction_read_only')", &[])
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::programming("the probe query returned no row"))?;
    let setting: String = row
        .try_get(0)
        .map_err(|e| Error::programming(format!("probe query: {e}")))?;
    Ok(setting == "on")
}

/// What the call ran with: `{"read_only", "intent_read", "lang", "tz", "companies", "keys"}`.
async fn probe_value(this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<Value> {
    let read_only = read_only(tx).await?;
    let env = this.as_records().env().clone();
    let context = env.context();
    let mut map = ValueMap::new();
    map.insert("read_only".into(), Value::Bool(read_only));
    map.insert("intent_read".into(), Value::Bool(tx.intent().is_read()));
    map.insert(
        "lang".into(),
        context.lang().map_or(Value::False, Value::str),
    );
    map.insert("tz".into(), context.tz().map_or(Value::False, Value::str));
    map.insert(
        "companies".into(),
        Value::list(env.companies()?.iter().map(|c| Value::Int(c.get()))),
    );
    map.insert(
        "keys".into(),
        Value::list(context.to_map().into_keys().map(Value::str)),
    );
    Ok(Value::Map(Arc::new(map)))
}

async fn create_named(
    this: &RecordSet<Effects>,
    tx: &mut Transaction,
    name: &str,
) -> Result<RecordSet<Effects>> {
    this.create(tx, vec![this.vals().set(Effects::NAME, name)])
        .await
}

fn serialization_failure() -> Error {
    Error::new(
        ErrorKind::Concurrency,
        "could not serialize access due to concurrent update",
    )
    .with_sqlstate(Some("40001".to_owned()))
}

#[methods(Effects, addon = "test_rpc")]
impl Effects {
    /// `read`: reports the transaction and context it ran with.
    #[method]
    #[rpc(effect = read, model)]
    async fn probe(this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<Value> {
        probe_value(this, tx).await
    }

    /// `write`: the same report from a read/write transaction.
    #[method]
    #[rpc(effect = write, model)]
    async fn probe_write(this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<Value> {
        probe_value(this, tx).await
    }

    /// Mislabelled `read` that creates.
    #[method]
    #[rpc(effect = read, model)]
    async fn bad_create(
        this: &RecordSet<Effects>,
        tx: &mut Transaction,
    ) -> Result<RecordSet<Effects>> {
        create_named(this, tx, "written by a read").await
    }

    /// Mislabelled `read` that writes.
    #[method]
    #[rpc(effect = read)]
    async fn bad_write(this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<bool> {
        this.write(tx, this.vals().set(Effects::NAME, "written by a read"))
            .await?;
        Ok(true)
    }

    /// Mislabelled `read` that deletes.
    #[method]
    #[rpc(effect = read)]
    async fn bad_unlink(this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<bool> {
        this.as_records().unlink(tx).await?;
        Ok(true)
    }

    /// Mislabelled `read` that locks rows.
    #[method]
    #[rpc(effect = read)]
    async fn bad_lock(this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<bool> {
        this.as_records().lock_for_update(tx, false).await?;
        Ok(true)
    }

    /// Mislabelled `read` that writes through raw SQL.
    #[method]
    #[rpc(effect = read, model)]
    async fn bad_sql(_this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<u64> {
        tx.execute("UPDATE test_rpc_effects SET counter = counter + 1", &[])
            .await
    }

    /// Mislabelled `read` that draws a sequence number (a `SELECT` the database refuses with
    /// `25006`).
    #[method]
    #[rpc(effect = read, model)]
    async fn bad_nextval(_this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<i64> {
        let rows = tx
            .query(
                "SELECT nextval(pg_get_serial_sequence('test_rpc_effects', 'id'))",
                &[],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::programming("nextval returned no row"))?;
        row.try_get::<_, i64>(0)
            .map_err(|e| Error::programming(format!("nextval: {e}")))
    }

    /// Creates one record named `name` (one effect per execution).
    #[method]
    #[rpc(effect = write, model)]
    async fn bump(
        this: &RecordSet<Effects>,
        tx: &mut Transaction,
        name: String,
    ) -> Result<RecordSet<Effects>> {
        create_named(this, tx, &name).await
    }

    /// Creates a record, then fails: nothing may stay.
    #[method]
    #[rpc(effect = write, model)]
    async fn bump_then_fail(
        this: &RecordSet<Effects>,
        tx: &mut Transaction,
        name: String,
    ) -> Result<i64> {
        create_named(this, tx, &name).await?;
        Err(Error::validation("bump refused"))
    }

    /// Creates a record and returns `size` bytes.
    #[method]
    #[rpc(effect = write, model)]
    async fn bump_big(
        this: &RecordSet<Effects>,
        tx: &mut Transaction,
        name: String,
        size: i64,
    ) -> Result<String> {
        create_named(this, tx, &name).await?;
        let size = usize::try_from(size).map_err(|_| Error::validation("size must be positive"))?;
        Ok("x".repeat(size))
    }

    /// Echoes its arguments: a required string, a defaulted integer and an optional boolean.
    #[method]
    #[rpc(effect = read, model)]
    async fn echo(
        _this: &RecordSet<Effects>,
        _tx: &mut Transaction,
        name: String,
        #[arg(default = 10)] limit: i64,
        flag: Option<bool>,
    ) -> Result<Value> {
        let mut map = ValueMap::new();
        map.insert("name".into(), Value::str(name));
        map.insert("limit".into(), Value::Int(limit));
        map.insert("flag".into(), flag.map_or(Value::False, Value::Bool));
        Ok(Value::Map(Arc::new(map)))
    }

    /// Binary in and out ('s codec half): `{"content": <base64>, "size": n}`.
    #[method]
    #[rpc(effect = read, model)]
    async fn binary_echo(
        _this: &RecordSet<Effects>,
        _tx: &mut Transaction,
        raw: Binary,
    ) -> Result<Value> {
        let size = i64::try_from(raw.0.len()).map_err(|_| Error::validation("too large"))?;
        let mut map = ValueMap::new();
        map.insert("content".into(), Value::Bytes(raw.0.into()));
        map.insert("size".into(), Value::Int(size));
        Ok(Value::Map(Arc::new(map)))
    }

    /// Not exposed: 403 over RPC.
    #[method]
    async fn internal_helper(_this: &RecordSet<Effects>, _tx: &mut Transaction) -> Result<bool> {
        Ok(true)
    }

    /// Exposed here, hidden by `test_rpc_ext`.
    #[method]
    #[rpc(effect = read, model)]
    async fn shown(_this: &RecordSet<Effects>, _tx: &mut Transaction) -> Result<bool> {
        Ok(true)
    }

    /// `read` here, widened to `external` by `test_rpc_ext`.
    #[method]
    #[rpc(effect = read, model)]
    async fn widened(_this: &RecordSet<Effects>, _tx: &mut Transaction) -> Result<bool> {
        Ok(true)
    }

    /// `read` here; `test_rpc_ext` overrides it without `#[rpc]` (the parent's
    /// mode holds). Returns whether it ran read-only.
    #[method]
    #[rpc(effect = read, model)]
    async fn inherited_read(_this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<bool> {
        read_only(tx).await
    }

    /// Creates a record named `name`, then sleeps `millis` before committing.
    #[method]
    #[rpc(effect = write, model)]
    async fn slow_bump(
        this: &RecordSet<Effects>,
        tx: &mut Transaction,
        name: String,
        millis: i64,
    ) -> Result<RecordSet<Effects>> {
        let created = create_named(this, tx, &name).await?;
        let secs = millis as f64 / 1000.0;
        tx.query("SELECT pg_sleep($1)::text", &[&secs]).await?;
        Ok(created)
    }

    /// Creates a record named `name`, then waits for the advisory lock `gate` (a test holds it
    /// until the race it stages has formed) before committing. Unlike `slow_bump` its length
    /// does not depend on the clock. The wait polls `pg_try_advisory_xact_lock`, so a short
    /// `lock_timeout` does not cut it off the way it would a plain lock wait.
    ///
    /// The wait is bounded by `millis` (default 30 s in the test callers) so a lone caller
    /// fails loudly naming `gate` if the test died before releasing it.
    #[method]
    #[rpc(effect = write, model)]
    async fn gated_bump(
        this: &RecordSet<Effects>,
        tx: &mut Transaction,
        name: String,
        gate: i64,
        #[arg(default = 30000)] millis: i64,
    ) -> Result<RecordSet<Effects>> {
        let millis = millis.max(0);
        let created = create_named(this, tx, &name).await?;
        tx.batch(&format!(
            "DO $$ DECLARE started timestamptz := clock_timestamp(); \
             BEGIN WHILE NOT pg_try_advisory_xact_lock({gate}) LOOP \
             IF clock_timestamp() - started >= interval '{millis} milliseconds' THEN \
                 RAISE EXCEPTION 'gated_bump: gate % was not released within % ms', {gate}, {millis}; \
             END IF; \
             PERFORM pg_sleep(0.005); END LOOP; END $$"
        ))
        .await?;
        Ok(created)
    }

    /// Reads `counter` (taking the snapshot), waits until a second caller has read too, then
    /// writes `counter + 1`: two concurrent calls conflict. The rendezvous is
    /// counted in [`READS`], not timed: only the first two reads wait (a retry runs alone).
    /// `millis` bounds the wait so a lone caller fails loudly instead of hanging.
    #[method]
    #[rpc(effect = write)]
    async fn slow_increment(
        this: &RecordSet<Effects>,
        tx: &mut Transaction,
        millis: i64,
    ) -> Result<i64> {
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        let current = this.get(tx, Effects::COUNTER).await?;
        if READS.fetch_add(1, Ordering::SeqCst) < 2 {
            let limit = std::time::Duration::from_millis(u64::try_from(millis).unwrap_or(0));
            let started = std::time::Instant::now();
            while READS.load(Ordering::SeqCst) < 2 {
                if started.elapsed() >= limit {
                    return Err(Error::validation(
                        "slow_increment: no second caller read the counter in time",
                    ));
                }
                tx.query("SELECT pg_sleep(0.005)::text", &[]).await?;
            }
        }
        let next = current
            .checked_add(1)
            .ok_or_else(|| Error::validation("counter overflow"))?;
        this.write(tx, this.vals().set(Effects::COUNTER, next))
            .await?;
        Ok(i64::from(next))
    }

    /// Always fails with a serialization failure.
    #[method]
    #[rpc(effect = write, model)]
    async fn always_conflict(_this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<bool> {
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        tx.batch("DO $$ BEGIN RAISE EXCEPTION 'conflict' USING ERRCODE = '40001'; END $$")
            .await?;
        Ok(true)
    }

    /// Creates a record and registers an after-commit hook; the first attempt fails with a
    /// serialization failure.
    #[method]
    #[rpc(effect = write, model)]
    async fn conflict_once(
        this: &RecordSet<Effects>,
        tx: &mut Transaction,
        name: String,
    ) -> Result<RecordSet<Effects>> {
        let attempt = ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        tx.after_commit(|| {
            HOOKS.fetch_add(1, Ordering::SeqCst);
        });
        let created = create_named(this, tx, &name).await?;
        if attempt == 0 {
            return Err(serialization_failure());
        }
        Ok(created)
    }

    /// Fails like a superseded ORM on its first attempt.
    #[method]
    #[rpc(effect = write, model)]
    async fn superseded_once(_this: &RecordSet<Effects>, _tx: &mut Transaction) -> Result<bool> {
        if ATTEMPTS.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(Error::new(ErrorKind::Concurrency, "the ORM was replaced")
                .with_sqlstate(Some("ERP01".to_owned()))
                .with_condition(Condition::OrmSuperseded));
        }
        Ok(true)
    }

    /// `read`, fails like a superseded ORM on its first attempt (a retry on a replaced registry).
    #[method]
    #[rpc(effect = read, model)]
    async fn superseded_read_once(
        _this: &RecordSet<Effects>,
        _tx: &mut Transaction,
    ) -> Result<bool> {
        if ATTEMPTS.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(Error::new(ErrorKind::Concurrency, "the ORM was replaced")
                .with_sqlstate(Some("ERP01".to_owned()))
                .with_condition(Condition::OrmSuperseded));
        }
        Ok(true)
    }

    /// Mislabelled `read` calling a function a read-only transaction allows but whose effect
    /// outlives the call: `set_config`, `pg_advisory_lock` or `pg_notify`.
    #[method]
    #[rpc(effect = read, model)]
    async fn bad_session(
        _this: &RecordSet<Effects>,
        tx: &mut Transaction,
        function: String,
    ) -> Result<bool> {
        let sql = match function.as_str() {
            "set_config" => "SELECT set_config('application_name', 'x', false)",
            "pg_advisory_lock" => "SELECT pg_advisory_lock(42)::text",
            "pg_notify" => "SELECT pg_notify('erp_test', '')::text",
            other => return Err(Error::validation(format!("unknown function {other}"))),
        };
        tx.query(sql, &[]).await?;
        Ok(true)
    }

    /// Locks its records with raw SQL, without waiting (`55P03` through the database error
    /// mapping).
    #[method]
    #[rpc(effect = write)]
    async fn locked_raw(this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<bool> {
        let ids: Vec<i64> = this.as_records().ids();
        tx.query(
            "SELECT id FROM test_rpc_effects WHERE id = ANY($1) FOR UPDATE NOWAIT",
            &[&ids],
        )
        .await?;
        Ok(true)
    }

    /// Locks its records without waiting (`55P03` when another transaction holds them).
    #[method]
    #[rpc(effect = write)]
    async fn locked(this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<bool> {
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        this.as_records().lock_for_update(tx, false).await?;
        Ok(true)
    }

    /// Fails in the database with a bound value the server quotes in its message.
    #[method]
    #[rpc(effect = write, model)]
    async fn secret_failure(
        _this: &RecordSet<Effects>,
        tx: &mut Transaction,
        secret: String,
    ) -> Result<bool> {
        tx.query("SELECT $1::text::int", &[&secret]).await?;
        Ok(true)
    }

    /// `destructive`: deletes its records.
    #[method]
    #[rpc(effect = destructive)]
    async fn burn(this: &RecordSet<Effects>, tx: &mut Transaction) -> Result<bool> {
        this.as_records().unlink(tx).await?;
        Ok(true)
    }

    /// `external`.
    #[method]
    #[rpc(effect = external, model)]
    async fn notify(_this: &RecordSet<Effects>, _tx: &mut Transaction) -> Result<bool> {
        Ok(true)
    }
}

/// Overrides of `test_rpc.effects` methods: hide, widen, inherit (RPC-001).
pub mod ext {
    use erp_sdk::prelude::*;

    use super::Effects;

    erp_sdk::addon!(name = "test_rpc_ext", depends = ["test_rpc"]);

    /// Extension of `test_rpc.effects`.
    #[extend(Effects, addon = "test_rpc_ext")]
    pub struct EffectsExt {}

    #[methods(Effects, addon = "test_rpc_ext")]
    impl EffectsExt {
        /// `@api.private` in a subclass: never callable over RPC.
        #[method(override)]
        #[rpc(hide)]
        async fn shown(
            this: &RecordSet<Effects>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<bool> {
            sup.call(tx, this.as_records(), ()).await
        }

        /// Widened to `external`.
        #[method(override)]
        #[rpc(effect = external, model)]
        async fn widened(
            this: &RecordSet<Effects>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<bool> {
            sup.call(tx, this.as_records(), ()).await
        }

        /// No `#[rpc]`: inherits `read`.
        #[method(override)]
        async fn inherited_read(
            this: &RecordSet<Effects>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<bool> {
            sup.call(tx, this.as_records(), ()).await
        }
    }
}

/// An override widening `superseded_read_once` from `read` to `write`: a registry with and one
/// without it model a server upgrade between two attempts of one call.
pub mod widen {
    use erp_sdk::prelude::*;

    use super::Effects;

    erp_sdk::addon!(name = "test_rpc_widen", depends = ["test_rpc"]);

    /// Extension of `test_rpc.effects`.
    #[extend(Effects, addon = "test_rpc_widen")]
    pub struct EffectsWiden {}

    #[methods(Effects, addon = "test_rpc_widen")]
    impl EffectsWiden {
        /// Widened to `write`.
        #[method(override)]
        #[rpc(effect = write, model)]
        async fn superseded_read_once(
            this: &RecordSet<Effects>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<bool> {
            sup.call(tx, this.as_records(), ()).await
        }
    }
}

/// An override narrowing `bump` from `write` to `read`: the ORM build fails (RPC-001).
pub mod narrow {
    use erp_sdk::prelude::*;

    use super::Effects;

    erp_sdk::addon!(name = "test_rpc_narrow", depends = ["test_rpc"]);

    /// Extension of `test_rpc.effects`.
    #[extend(Effects, addon = "test_rpc_narrow")]
    pub struct EffectsNarrow {}

    #[methods(Effects, addon = "test_rpc_narrow")]
    impl EffectsNarrow {
        /// Declares `read` below `write`.
        #[method(override)]
        #[rpc(effect = read, model)]
        async fn bump(
            this: &RecordSet<Effects>,
            tx: &mut Transaction,
            name: String,
            sup: Super,
        ) -> Result<RecordSet<Effects>> {
            sup.call(tx, this.as_records(), (name,)).await
        }
    }
}
