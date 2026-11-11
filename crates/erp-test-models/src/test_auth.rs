//! Fixture of the session-end hook tests (lead
//! note T-PUSH.8): a model that keeps one row per session, like push subscriptions or devices,
//! and a hook that deletes them when their sessions end. A row named `refuse` makes the hook
//! fail, so the test can check that a failing hook fails the deletion; a row named `records` makes
//! it use the record API, which every hook invocation must refuse.
//!
//! It also overrides `res.users._check_credentials` (review round 1), so every login
//! of these tests takes the overridden path, where the login keeps its first transaction open for
//! the override: a user whose login starts with `gated` is refused after the password check, one
//! whose login starts with `broken` fails the check with a `Validation` error, and
//! the override records each check it runs in `test_auth_check` (committed with a success, rolled
//! back with a refusal).
//!
//! A user whose login starts with `poolmem` fails the check with the hashing pool's
//! `MemoryOverReservation` refusal, as an override that hashes through the pool itself with more
//! memory than a permit reserves would (LD2: the login's failure
//! accounting counts that refusal).
//!
//! The hook also records, in `test_auth.ended`, why each batch of sessions ended and the isolation
//! level of the transaction it ran in (security review round 2 N7, database review round 2 N9: the
//! session ends run at READ COMMITTED).

use erp_sdk::BoxFuture;
use erp_sdk::auth::{AuthInfo, CheckEnv, Credential, SessionEnd, access_denied};
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_auth", depends = ["base"]);

/// A row per session.
#[model(
    name = "test_auth.device",
    description = "Auth Device",
    addon = "test_auth"
)]
pub struct Device {
    /// The session's id (text form).
    #[field(char, required = true)]
    pub session_id: Option<Arc<str>>,
    /// A label; `refuse` makes the hook fail.
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(Device, addon = "test_auth")]
impl Device {
    /// A method whose continuation is called inside a raw-only scope by the fixture override.
    #[method]
    async fn _raw_scope_probe(_this: &RecordSet<Device>, _tx: &mut Transaction) -> Result<bool> {
        Ok(true)
    }
}

/// A separate addon owns the override: one implementation per addon in a method chain.
pub mod scope_override {
    use super::*;

    erp_sdk::addon!(name = "test_auth_scope", depends = ["test_auth"]);

    /// Exercises a method continuation constructed before entering a raw-only scope.
    #[extend(Device, addon = "test_auth_scope")]
    pub struct ScopedDevice {}

    #[methods(Device, addon = "test_auth_scope")]
    impl ScopedDevice {
        /// The continuation must refuse before invoking the base implementation.
        #[method(override)]
        async fn _raw_scope_probe(
            this: &RecordSet<Device>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<bool> {
            let mut raw = tx.raw_statements_only();
            sup.call(&mut raw, this.as_records(), ()).await
        }
    }
}

/// One row per run of the hook: why the sessions ended and the isolation of its transaction.
#[model(
    name = "test_auth.ended",
    description = "Auth Ended Sessions",
    addon = "test_auth"
)]
pub struct Ended {
    /// The end reason (`Debug` text of `EndReason`).
    #[field(char)]
    pub reason: Option<Arc<str>>,
    /// `current_setting('transaction_isolation')` in the hook's transaction.
    #[field(char)]
    pub isolation: Option<Arc<str>>,
}

erp_sdk::auth::session_end_hook!(DROP_DEVICES, addon = "test_auth", run = drop_devices);

fn drop_devices<'a>(tx: &'a mut Transaction, ended: &'a SessionEnd) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let ids: Vec<String> = ended.sessions().iter().map(|s| s.id().to_text()).collect();
        let refused = tx
            .query(
                "SELECT 1 FROM test_auth_device WHERE session_id = ANY($1) AND name = 'refuse'",
                &[&ids],
            )
            .await?;
        if !refused.is_empty() {
            return Err(Error::validation("test_auth refuses to end this session"));
        }
        // a row named `records` makes the hook break the raw-statements-only contract: it reads
        // through the record API (refused at every isolation level).
        let misuse = tx
            .query(
                "SELECT 1 FROM test_auth_device WHERE session_id = ANY($1) AND name = 'records'",
                &[&ids],
            )
            .await?;
        if !misuse.is_empty() {
            let devices = tx.env().records("test_auth.device")?;
            devices.search(tx, &erp_sdk::Domain::TRUE).await?;
        }
        tx.execute(
            "DELETE FROM test_auth_device WHERE session_id = ANY($1)",
            &[&ids],
        )
        .await?;
        let reason = format!("{:?}", ended.reason());
        tx.execute(
            "INSERT INTO test_auth_ended (reason, isolation) \
             VALUES ($1, current_setting('transaction_isolation'))",
            &[&reason],
        )
        .await?;
        Ok(())
    })
}

/// The `_check_credentials` override of this addon.
#[extend(ResUsers, addon = "test_auth")]
pub struct UsersLoginGate {}

#[methods(ResUsers, addon = "test_auth")]
impl UsersLoginGate {
    /// `super`, then refuse logins starting with `gated`; record the check either way.
    #[method(override)]
    async fn _check_credentials(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        credential: Arc<Credential>,
        env: CheckEnv,
        sup: Super,
    ) -> Result<AuthInfo> {
        if let Credential::Password { login, .. } = &*credential
            && login.starts_with("poolmem")
        {
            let pool = env.pool();
            // refused before it runs: the job needs more memory than a permit reserves
            pool.run(pool.config().memory_kib() + 1, || ()).await?;
        }
        let info: AuthInfo = sup
            .call(tx, this.as_records(), (Arc::clone(&credential), env))
            .await?;
        tx.batch("CREATE TABLE IF NOT EXISTS test_auth_check (uid int8 NOT NULL)")
            .await?;
        tx.execute(
            "INSERT INTO test_auth_check (uid) VALUES ($1)",
            &[&info.uid().get()],
        )
        .await?;
        let login = match &*credential {
            Credential::Password { login, .. } => login.as_str(),
            _ => "",
        };
        if login.starts_with("gated") {
            return Err(access_denied());
        }
        if login.starts_with("broken") {
            return Err(Error::validation("test_auth cannot check this credential"));
        }
        Ok(info)
    }
}
