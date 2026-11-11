//! The in-process RPC harness (design note §17): an [`RpcService`] over a
//! throwaway database, always under a restricting access policy. [`StaticPolicy::AllowAll`] is
//! refused: RPC tests must show that the gates and the policy hold, which an allow-all policy
//! hides.
//!
//! ```ignore
//! let h = RpcHarness::builder(Harness::builder([&my_addon::ADDON]), MyPolicy)
//!     .host(MyHost)
//!     .build()
//!     .await?;
//! let response = h.service().call(RpcCall::json2("/json/2/m.x/search", br#"{"domain":[]}"#.to_vec(), caller)).await;
//! ```

use std::any::Any;
use std::sync::Arc;

use erp_orm::signal::{Changes, Refresh, Refreshed, Signal};
use erp_orm::{BoxFuture, IntoPolicyProvider, Orm, PolicyProvider, StaticPolicy};
use erp_rpc::{RpcConfig, RpcHost, RpcService};

use crate::harness::{Harness, HarnessBuilder, HarnessError};

/// Builds the service's host from the ORM.
type HostFactory = Box<dyn FnOnce(&Orm) -> Box<dyn RpcHost> + Send>;

/// Builds an [`RpcHarness`].
#[must_use = "call `.build().await`"]
pub struct RpcHarnessBuilder {
    harness: HarnessBuilder,
    policy: Result<Arc<dyn PolicyProvider>, HarnessError>,
    host: Option<HostFactory>,
    config: RpcConfig,
    signal: bool,
    read_pool_options: Option<String>,
}

/// A throwaway database, its ORM and an [`RpcService`] over it.
pub struct RpcHarness {
    harness: Harness,
    orm: Orm,
    service: RpcService,
}

/// A refresh hook that applies policy and registry bumps in place (the test registry never
/// changes shape).
struct InPlace;

impl Refresh for InPlace {
    fn refresh<'a>(
        &'a self,
        _changes: &'a Changes,
    ) -> BoxFuture<'a, erp_fields::Result<Refreshed>> {
        Box::pin(async { Ok(Refreshed::InPlace) })
    }
}

/// Refuse [`StaticPolicy::AllowAll`] (also behind an `Arc`).
fn refuse_allow_all<P: Any>(policy: &P) -> Result<(), HarnessError> {
    let any: &dyn Any = policy;
    let allow_all = any.downcast_ref::<StaticPolicy>() == Some(&StaticPolicy::AllowAll)
        || any
            .downcast_ref::<Arc<StaticPolicy>>()
            .is_some_and(|p| **p == StaticPolicy::AllowAll);
    if allow_all {
        return Err(HarnessError::Orm(erp_fields::Error::programming(
            "RpcHarness refuses StaticPolicy::AllowAll: RPC tests run under a restricting \
             policy",
        )));
    }
    Ok(())
}

impl RpcHarness {
    /// Start from a harness builder (its addons, models, impls) and the restricting `policy`
    /// the harness and the service use.
    pub fn builder<P: IntoPolicyProvider + Any>(
        harness: HarnessBuilder,
        policy: P,
    ) -> RpcHarnessBuilder {
        let policy = refuse_allow_all(&policy)
            .and_then(|()| policy.into_policy_provider().map_err(HarnessError::Orm));
        RpcHarnessBuilder {
            harness,
            policy,
            host: None,
            config: RpcConfig::default(),
            signal: false,
            read_pool_options: None,
        }
    }

    /// The service.
    pub fn service(&self) -> &RpcService {
        &self.service
    }

    /// The ORM the service runs on.
    pub fn orm(&self) -> &Orm {
        &self.orm
    }

    /// The underlying harness (database, system transactions).
    pub fn harness(&self) -> &Harness {
        &self.harness
    }

    /// Drop the database.
    pub async fn cleanup(self) -> Result<(), HarnessError> {
        drop(self.service);
        drop(self.orm);
        self.harness.cleanup().await
    }
}

impl RpcHarnessBuilder {
    /// The host (languages, capabilities), built from the ORM.
    pub fn host<H: RpcHost>(mut self, host: impl FnOnce(&Orm) -> H + Send + 'static) -> Self {
        self.host = Some(Box::new(move |orm| Box::new(host(orm)) as Box<dyn RpcHost>));
        self
    }

    /// The service's settings.
    pub fn config(mut self, config: RpcConfig) -> Self {
        self.config = config;
        self
    }

    /// Run the service on an ORM with cross-process signals, whose refresh hook
    /// applies policy bumps in place: idempotency replays then see policy generations.
    pub fn with_signal(mut self) -> Self {
        self.signal = true;
        self
    }

    /// Give the ORM a read pool of connections to the same database, opened with
    /// the PostgreSQL startup `options` (for example `-c application_name=read_pool`), so a test
    /// can tell which pool served a transaction. The pool logs in as a read-only role
    /// ([`Harness::read_only_role`]), as the ORM requires of a read pool. Needs
    /// [`RpcHarnessBuilder::with_signal`].
    pub fn with_read_pool(mut self, options: impl Into<String>) -> Self {
        self.read_pool_options = Some(options.into());
        self
    }

    /// Build the database, the schema (models, then core tables through `sync_schema`) and the
    /// service.
    ///
    /// # Errors
    ///
    /// `Programming` for [`StaticPolicy::AllowAll`] or a missing host; the harness's errors.
    pub async fn build(self) -> Result<RpcHarness, HarnessError> {
        let policy = self.policy?;
        let Some(host) = self.host else {
            return Err(HarnessError::Orm(erp_fields::Error::programming(
                "RpcHarness needs a host: call .host(...)",
            )));
        };
        let harness = self.harness.policy(Arc::clone(&policy)).build().await?;
        let orm = if self.signal {
            let mut tx = harness.orm().begin(erp_orm::SUPERUSER_ID).await?;
            tx.install_signal().await?;
            let baseline = tx.signal_generations().await?;
            tx.commit().await?;
            let mut builder =
                Orm::builder(Arc::clone(harness.registry()), harness.db().pool().clone())
                    .policy(Arc::clone(&policy))
                    .signal(Signal::new(baseline).with_refresh(InPlace));
            if let Some(options) = &self.read_pool_options {
                // the login is made after the schema below; connections open lazily, after it
                let mut config = harness.read_only_config();
                config.options(options);
                let pool = erp_orm::pool::create_pool(config, 4).map_err(|e| {
                    HarnessError::Orm(erp_fields::Error::programming(format!(
                        "read pool of the RPC harness: {e}"
                    )))
                })?;
                // no signal listener runs in the harness: the lag bound stands in for it
                builder = builder
                    .read_pool(pool)
                    .read_pool_max_lag(std::time::Duration::from_secs(60));
            }
            builder.build()?
        } else if self.read_pool_options.is_some() {
            return Err(HarnessError::Orm(erp_fields::Error::programming(
                "RpcHarnessBuilder::with_read_pool needs with_signal",
            )));
        } else {
            harness.orm().clone()
        };
        let report = orm.sync_schema().await?;
        if !report.is_empty() {
            return Err(HarnessError::Orm(erp_fields::Error::programming(format!(
                "the core tables' schema step left post-commit work: {report:?}"
            ))));
        }
        if self.read_pool_options.is_some() {
            harness.read_only_role().await?;
        }
        let host = host(&orm);
        let service = RpcService::new(orm.clone(), BoxedHost(host), self.config);
        Ok(RpcHarness {
            harness,
            orm,
            service,
        })
    }
}

struct BoxedHost(Box<dyn RpcHost>);

impl RpcHost for BoxedHost {
    fn language_active(&self, code: &str) -> erp_fields::Result<bool> {
        self.0.language_active(code)
    }

    fn has_capability(
        &self,
        principal: &erp_orm::Principal,
        capability: &erp_orm::Capability,
    ) -> erp_fields::Result<bool> {
        self.0.has_capability(principal, capability)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use super::*;

    #[test]
    fn allow_all_is_refused_directly_and_behind_an_arc() {
        for result in [
            refuse_allow_all(&StaticPolicy::AllowAll),
            refuse_allow_all(&Arc::new(StaticPolicy::AllowAll)),
        ] {
            match result.unwrap_err() {
                HarnessError::Orm(e) => assert_eq!(
                    e.message(),
                    "RpcHarness refuses StaticPolicy::AllowAll: RPC tests run under a \
                     restricting policy"
                ),
                other => panic!("unexpected {other}"),
            }
        }
        refuse_allow_all(&StaticPolicy::SuperuserOnly).unwrap();
    }
}
