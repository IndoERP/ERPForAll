//! Fixture of the login cooldown policy tests: an override of
//! `res.users._on_login_cooldown` whose answer depends on the submitted login's prefix.
//!
//! - `strict…`: after `super`, also puts the login on cooldown after 2 failures for 60 s.
//! - `addr…`: also puts the client address on cooldown after 2 failures for 60 s.
//! - `weak…`: asks for the weakest legal rule (threshold 4000000), which cannot loosen the core one.
//! - `hookerr…`: fails with a `Validation` error whose text must not reach the caller.
//! - `hookpanic…`: panics.
//! - `badrule…`: builds a rule with threshold 0.
//! - `writer…`: tries to write (the hook's transaction is read-only).
//! - `hookslow…`: waits forever (an async wait): only the deadline or a hung-up caller ends it;
//!   [`dropped`] counts, per login, how often the wait was cancelled (its future dropped); [`calls`]
//!   tells how often it began.
//! - `flaky…`: answers `super`'s policy on its first call per login and fails on every later one
//!   (the restart path asks again).
//! - anything else: `super`'s policy unchanged.
//!
//! Every call is counted per submitted login ([`calls`]), so a test can tell whether the hook ran
//! for its login (tests of one binary run in parallel).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use erp_sdk::auth::{LoginCooldownContext, LoginCooldownPolicy};
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_cooldown", depends = ["base"]);

static CALLS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// How many times the hook ran for `login`.
pub fn calls(login: &str) -> u64 {
    CALLS
        .lock()
        .map(|m| m.as_ref().and_then(|m| m.get(login)).copied().unwrap_or(0))
        .unwrap_or(0)
}

static DROPPED: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// How many `hookslow` waits of `login` were cancelled (their future dropped).
pub fn dropped(login: &str) -> u64 {
    DROPPED
        .lock()
        .map(|m| m.as_ref().and_then(|m| m.get(login)).copied().unwrap_or(0))
        .unwrap_or(0)
}

struct CountDrop(String);

impl Drop for CountDrop {
    fn drop(&mut self) {
        if let Ok(mut m) = DROPPED.lock() {
            *m.get_or_insert_with(HashMap::new)
                .entry(std::mem::take(&mut self.0))
                .or_insert(0) += 1;
        }
    }
}

/// The `_on_login_cooldown` override of this addon.
#[extend(ResUsers, addon = "test_cooldown")]
pub struct UsersCooldown {}

#[methods(ResUsers, addon = "test_cooldown")]
impl UsersCooldown {
    /// `super`, then the rule the login's prefix asks for.
    #[method(override)]
    #[allow(
        clippy::panic,
        reason = "the fixture panics on purpose (hookpanic logins)"
    )]
    async fn _on_login_cooldown(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        context: LoginCooldownContext,
        sup: Super,
    ) -> Result<LoginCooldownPolicy> {
        let login = context.login().to_owned();
        let nth = CALLS.lock().map_or(1, |mut m| {
            let n = m
                .get_or_insert_with(HashMap::new)
                .entry(login.clone())
                .or_insert(0);
            *n += 1;
            *n
        });
        let policy: LoginCooldownPolicy = sup.call(tx, this.as_records(), (context,)).await?;
        let minute = Duration::from_secs(60);
        if login.starts_with("strict") {
            return policy.with_login_rule(2, minute);
        }
        if login.starts_with("addr") {
            return policy.with_address_rule(2, minute);
        }
        if login.starts_with("weak") {
            return policy.with_login_rule(4_000_000, Duration::from_secs(1));
        }
        if login.starts_with("hookerr") {
            return Err(Error::validation("hook secret detail"));
        }
        if login.starts_with("hookpanic") {
            panic!("hook secret panic");
        }
        if login.starts_with("hookslow") {
            let _guard = CountDrop(login.clone());
            std::future::pending::<()>().await;
        }
        if login.starts_with("flaky") && nth > 1 {
            return Err(Error::validation("hook secret flaky detail"));
        }
        if login.starts_with("badrule") {
            return policy.with_login_rule(0, minute);
        }
        if login.starts_with("writer") {
            tx.batch("CREATE TABLE test_cooldown_write (x int)").await?;
        }
        Ok(policy)
    }
}
