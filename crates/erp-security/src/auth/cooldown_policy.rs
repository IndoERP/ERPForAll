//! What `res.users._on_login_cooldown(context)` takes and returns (H58 cooldown).
//!
//! `_on_login_cooldown` is a dispatched method chain on `res.users`: the `base` addon defines it
//! and returns [`LoginCooldownPolicy::base`]; an addon may override it and call `super`, and add
//! *stricter* rules. The result can only tighten: the core cooldown (`base.login_cooldown_*`,
//! evaluated on the shared `erp_login_throttle` rows) always applies, and the attempt is refused
//! when the core rule **or** an additional rule is in force. A policy cannot disable a core rule,
//! accept a credential, or keep a counter of its own.
//!
//! The hook runs in a read-only transaction that is always rolled back, before any credential work,
//! and receives only the submitted login and the client address: never the user's id, the stored
//! hash or the password. A hook that looks the login up in the database makes known and unknown
//! logins take different times; that is the addon author's to avoid.

use std::net::IpAddr;
use std::time::Duration;

use erp_fields::{Error, Result};

use super::login::ClientAddr;
use super::settings::Cooldown;

/// Largest failure threshold an additional address rule may use (the setting's own range).
const ADDRESS_AFTER_MAX: u32 = 1_000_000;
/// Largest failure threshold an additional login rule may use (the setting's own range).
const LOGIN_AFTER_MAX: u32 = 4_000_000;
/// Longest cooldown an additional rule may use, seconds (the setting's own range).
const DURATION_MAX_SECS: u64 = 604_800;

/// What the cooldown hook is told about the attempt.
#[derive(Debug, Clone)]
pub struct LoginCooldownContext {
    login: String,
    client: ClientAddr,
    address_cooldown: Cooldown,
    login_cooldown: Cooldown,
}

impl LoginCooldownContext {
    pub(crate) fn new(
        login: &str,
        client: ClientAddr,
        address_cooldown: Cooldown,
        login_cooldown: Cooldown,
    ) -> Self {
        Self {
            login: login.to_owned(),
            client,
            address_cooldown,
            login_cooldown,
        }
    }

    /// The submitted login, as typed (it may name no user). For an identity confirmation of a
    /// logged-in user it is that user's stored login.
    pub fn login(&self) -> &str {
        &self.login
    }

    /// The client address, `None` for an in-process caller.
    pub fn address(&self) -> Option<IpAddr> {
        match self.client {
            ClientAddr::Known(addr) => Some(addr),
            ClientAddr::InProcess(_) => None,
        }
    }

    /// Failures per client address before the core cooldown, and its length.
    pub fn address_rule(&self) -> (u32, Duration) {
        (
            self.address_cooldown.after(),
            self.address_cooldown.duration(),
        )
    }

    /// Failures per login before the core cooldown (0: the core rule is off), and its length.
    pub fn login_rule(&self) -> (u32, Duration) {
        (self.login_cooldown.after(), self.login_cooldown.duration())
    }
}

/// The additional restrictions a cooldown hook asks for; [`Self::base`] asks for none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginCooldownPolicy {
    address: Option<Cooldown>,
    login: Option<Cooldown>,
}

impl LoginCooldownPolicy {
    /// No additional rule: the core cooldown alone.
    pub const fn base() -> Self {
        Self {
            address: None,
            login: None,
        }
    }

    /// Also put a client address on cooldown after `after` failures, for `duration`. Combined with
    /// a rule this policy already has, the stricter of the two stands (the lower threshold, the
    /// longer duration), so a `super` chain can only tighten. Needs an actual client address to
    /// apply.
    ///
    /// # Errors
    ///
    /// `Validation` naming the field for a threshold outside `1..=1_000_000` or a duration that
    /// is not a whole number of seconds in `1..=604_800`.
    pub fn with_address_rule(self, after: u32, duration: Duration) -> Result<Self> {
        let rule = merge(self.address, "address", after, ADDRESS_AFTER_MAX, duration)?;
        Ok(Self {
            address: Some(rule),
            ..self
        })
    }

    /// Also put a login on cooldown after `after` failures, for `duration` (a trusted
    /// (address, login) pair is still exempt, as for the core rule). Combination and bounds as
    /// [`Self::with_address_rule`], threshold `1..=4_000_000`.
    ///
    /// # Errors
    ///
    /// `Validation` naming the field, as [`Self::with_address_rule`].
    pub fn with_login_rule(self, after: u32, duration: Duration) -> Result<Self> {
        let rule = merge(self.login, "login", after, LOGIN_AFTER_MAX, duration)?;
        Ok(Self {
            login: Some(rule),
            ..self
        })
    }

    pub(crate) fn address(&self) -> Option<Cooldown> {
        self.address
    }

    pub(crate) fn login(&self) -> Option<Cooldown> {
        self.login
    }
}

fn merge(
    current: Option<Cooldown>,
    field: &str,
    after: u32,
    after_max: u32,
    duration: Duration,
) -> Result<Cooldown> {
    if after == 0 || after > after_max {
        return Err(Error::validation(format!(
            "_on_login_cooldown: the {field} rule's threshold must be 1..={after_max}, not {after}"
        )));
    }
    let secs = duration.as_secs();
    if duration.subsec_nanos() != 0 || secs == 0 || secs > DURATION_MAX_SECS {
        return Err(Error::validation(format!(
            "_on_login_cooldown: the {field} rule's duration must be a whole number of seconds \
             in 1..={DURATION_MAX_SECS}, not {duration:?}"
        )));
    }
    Ok(match current {
        None => Cooldown::new(after, duration),
        Some(c) => Cooldown::new(after.min(c.after()), duration.max(c.duration())),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use erp_fields::ErrorKind;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn base_asks_for_nothing() {
        let p = LoginCooldownPolicy::base();
        assert!(p.address().is_none() && p.login().is_none());
    }

    #[test]
    fn bounds_name_the_field() {
        for (r, field) in [
            (
                LoginCooldownPolicy::base().with_address_rule(0, secs(1)),
                "address",
            ),
            (
                LoginCooldownPolicy::base().with_address_rule(1_000_001, secs(1)),
                "address",
            ),
            (
                LoginCooldownPolicy::base().with_login_rule(4_000_001, secs(1)),
                "login",
            ),
            (
                LoginCooldownPolicy::base().with_login_rule(1, secs(0)),
                "login",
            ),
            (
                LoginCooldownPolicy::base().with_login_rule(1, secs(604_801)),
                "login",
            ),
            (
                LoginCooldownPolicy::base().with_login_rule(1, Duration::from_millis(1500)),
                "login",
            ),
        ] {
            let e = r.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Validation);
            assert!(
                e.message()
                    .starts_with(&format!("_on_login_cooldown: the {field} rule's")),
                "{}",
                e.message()
            );
        }
        let max = LoginCooldownPolicy::base()
            .with_address_rule(1_000_000, secs(604_800))
            .unwrap()
            .with_login_rule(4_000_000, secs(604_800))
            .unwrap();
        assert_eq!(max.address().unwrap().after(), 1_000_000);
        assert_eq!(max.login().unwrap().after(), 4_000_000);
    }

    #[test]
    fn chained_address_rules_only_tighten() {
        let p = LoginCooldownPolicy::base()
            .with_address_rule(5, secs(60))
            .unwrap()
            .with_address_rule(9, secs(600))
            .unwrap()
            .with_address_rule(3, secs(10))
            .unwrap();
        let c = p.address().unwrap();
        assert_eq!((c.after(), c.duration()), (3, secs(600)));
    }

    #[test]
    fn chained_rules_only_tighten() {
        let p = LoginCooldownPolicy::base()
            .with_login_rule(5, secs(60))
            .unwrap()
            .with_login_rule(9, secs(600))
            .unwrap()
            .with_login_rule(3, secs(10))
            .unwrap();
        let c = p.login().unwrap();
        assert_eq!((c.after(), c.duration()), (3, secs(600)));
    }
}
