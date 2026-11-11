//! Test fixture addon for login candidate selection hooks.
//!
//! Overrides `res.users._get_login_domain` and `res.users._get_login_order`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test fixture"
)]

use std::sync::Arc;

use erp_sdk::auth::{
    AuthInfo, CheckEnv, Credential, LoginDomain, LoginLookup, LoginOrder, LoginOrderTerm,
};
use erp_sdk::prelude::*;
use erp_sdk::{Domain, Operator};

erp_sdk::addon!(name = "test_login_selection", depends = ["base"]);

/// The domain the next `inject:` lookups return (the LOGIN-MATRICES type/null matrices store the
/// cell's domain here, then log in; the whole admission, optimization, compile and execution path
/// then runs on it). `None` outside those tests.
pub static INJECTED_DOMAIN: std::sync::Mutex<Option<Domain>> = std::sync::Mutex::new(None);

/// The order terms the order hook returns while set (`None`: `priority DESC`).
pub static INJECTED_ORDER: std::sync::Mutex<Option<Vec<LoginOrderTerm>>> =
    std::sync::Mutex::new(None);

/// Test model extension adding `alias` and `priority` to `res.users`.
#[extend(ResUsers, addon = "test_login_selection")]
pub struct UsersLoginSelection {
    #[field(char)]
    pub alias: Option<Arc<str>>,
    #[field(integer)]
    pub priority: Option<i32>,
    #[field(boolean)]
    pub selection_flag: bool,
}

#[methods(ResUsers, addon = "test_login_selection")]
impl UsersLoginSelection {
    #[method(override)]
    async fn _get_login_domain(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        lookup: LoginLookup,
        sup: Super,
    ) -> Result<LoginDomain> {
        let input = lookup.as_str();
        if let Some(alias) = input.strip_prefix("boolfalse:") {
            let alias = Domain::condition("alias", Operator::Eq, alias)
                .map_err(|e| Error::validation(e.to_string()))?;
            let flag = Domain::condition("selection_flag", Operator::Eq, false)
                .map_err(|e| Error::validation(e.to_string()))?;
            return Ok(LoginDomain::custom(Domain::and([alias, flag])));
        }
        if input.starts_with("inject:") {
            let injected = INJECTED_DOMAIN
                .lock()
                .map_err(|_| Error::programming("the injected login domain slot is poisoned"))?
                .clone();
            return injected
                .map(LoginDomain::custom)
                .ok_or_else(|| Error::programming("no login domain was injected for this test"));
        }
        if let Some(alias) = input.strip_prefix("alias:") {
            let domain = Domain::condition("alias", Operator::Eq, alias)
                .map_err(|e| Error::validation(e.to_string()))?;
            return Ok(LoginDomain::custom(domain));
        }
        if let Some(alias) = input.strip_prefix("order_desc:") {
            let domain = Domain::condition("alias", Operator::Eq, alias)
                .map_err(|e| Error::validation(e.to_string()))?;
            return Ok(LoginDomain::custom(domain));
        }
        if input.starts_with("query_effect:") {
            // Illegal side-effect inside hook: query execution must trigger ForbiddenEffect
            let _ = tx.query("SELECT 1", &[]).await?;
            return LoginDomain::exact_login(input);
        }
        if input.starts_with("panic_hook:") {
            panic!("selection hook intentionally exploded");
        }
        if input.starts_with("bad_op:") {
            let domain = Domain::condition("alias", Operator::ILike, "test")
                .map_err(|e| Error::validation(e.to_string()))?;
            return Ok(LoginDomain::custom(domain));
        }
        if input.starts_with("bad_field:") {
            let domain = Domain::condition("nonexistent_column", Operator::Eq, "val")
                .map_err(|e| Error::validation(e.to_string()))?;
            return Ok(LoginDomain::custom(domain));
        }
        if input.starts_with("relational_field:") {
            let domain = Domain::condition("company_id.name", Operator::Eq, "val")
                .map_err(|e| Error::validation(e.to_string()))?;
            return Ok(LoginDomain::custom(domain));
        }
        if input.starts_with("deep_domain:") {
            let mut d = Domain::condition("login", Operator::Eq, "user")
                .map_err(|e| Error::validation(e.to_string()))?;
            for i in 0..18 {
                if i % 2 == 0 {
                    d = Domain::or([
                        d,
                        Domain::condition("login", Operator::Eq, format!("u{i}"))
                            .map_err(|e| Error::validation(e.to_string()))?,
                    ]);
                } else {
                    d = Domain::and([
                        d,
                        Domain::condition("active", Operator::Eq, true)
                            .map_err(|e| Error::validation(e.to_string()))?,
                    ]);
                }
            }
            return Ok(LoginDomain::custom(d));
        }
        if input.starts_with("too_many_nodes:") {
            let mut children = Vec::with_capacity(1025);
            for i in 0..1025 {
                children.push(
                    Domain::condition("priority", Operator::Eq, i as i64)
                        .map_err(|e| Error::validation(e.to_string()))?,
                );
            }
            let d = Domain::or(children);
            return Ok(LoginDomain::custom(d));
        }
        if input.starts_with("super_chain:") {
            let clean = input.strip_prefix("super_chain:").unwrap();
            let sub = LoginLookup::new(clean, 1024)?;
            return sup.call(tx, this.as_records(), (sub,)).await;
        }

        sup.call(tx, this.as_records(), (lookup,)).await
    }

    #[method(override)]
    async fn _get_login_order(
        this: &RecordSet<ResUsers>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<LoginOrder> {
        let _ = (this, tx, sup);
        let injected = INJECTED_ORDER
            .lock()
            .map_err(|_| Error::programming("the injected login order slot is poisoned"))?
            .clone();
        Ok(LoginOrder::custom(
            injected.unwrap_or_else(|| vec![LoginOrderTerm::desc("priority")]),
        ))
    }
}

/// Sub-addon defining both selection and credential override (LC-14).
pub mod credential_override {
    use super::*;

    erp_sdk::addon!(
        name = "test_login_selection_override",
        depends = ["test_login_selection"]
    );

    #[extend(ResUsers, addon = "test_login_selection_override")]
    pub struct CredentialOverrideUsers {}

    #[methods(ResUsers, addon = "test_login_selection_override")]
    impl CredentialOverrideUsers {
        #[method(override)]
        async fn _check_credentials(
            this: &RecordSet<ResUsers>,
            tx: &mut Transaction,
            credential: Arc<Credential>,
            env: CheckEnv,
            sup: Super,
        ) -> Result<AuthInfo> {
            sup.call(tx, this.as_records(), (credential, env)).await
        }
    }
}

/// Domain-only override for startup admission tests.
pub mod domain_only {
    use super::*;

    erp_sdk::addon!(name = "test_login_domain_only", depends = ["base"]);

    #[extend(ResUsers, addon = "test_login_domain_only")]
    pub struct DomainOnlyUsers {}

    #[methods(ResUsers, addon = "test_login_domain_only")]
    impl DomainOnlyUsers {
        #[method(override)]
        async fn _get_login_domain(
            this: &RecordSet<ResUsers>,
            tx: &mut Transaction,
            lookup: LoginLookup,
            sup: Super,
        ) -> Result<LoginDomain> {
            sup.call(tx, this.as_records(), (lookup,)).await
        }
    }
}

/// Order-only override for startup admission tests.
pub mod order_only {
    use super::*;

    erp_sdk::addon!(name = "test_login_order_only", depends = ["base"]);

    #[extend(ResUsers, addon = "test_login_order_only")]
    pub struct OrderOnlyUsers {}

    #[methods(ResUsers, addon = "test_login_order_only")]
    impl OrderOnlyUsers {
        #[method(override)]
        async fn _get_login_order(
            this: &RecordSet<ResUsers>,
            tx: &mut Transaction,
            sup: Super,
        ) -> Result<LoginOrder> {
            sup.call(tx, this.as_records(), ()).await
        }
    }
}
