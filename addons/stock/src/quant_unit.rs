// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native whole-unit boundary for stock (4/D9).
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use erp_addon_base::ir_config_setting::read_values;
use erp_sdk::views::wire::{SettingValue, SettingsScope};
use erp_sdk::{
    BeginOptions, Context, Error, ErrorKind, OperationIdentity, Orm, PrincipalHint, Result,
    RetryPolicy, RunOptions, Transaction, require_top_level_unit,
};

use crate::settings::QUANT_KEYS;

const ENTRY: &str = "stock.run_quant_unit";

/// Immutable inputs of a native stock invocation. The entry point owns access checks on its
/// documents and the durable idempotency protocol. Identity only correlates its attempts.
///
/// ```
/// use erp_addon_stock::QuantUnit;
/// use erp_sdk::{OperationIdentity, PrincipalHint, UserId};
/// let caller = PrincipalHint::caller(UserId::new(42)?)?;
/// let unit = QuantUnit::new(caller, OperationIdentity::generate()?, "Blue widget")?;
/// # Ok::<(), erp_sdk::Error>(())
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct QuantUnit {
    principal: PrincipalHint,
    identity: OperationIdentity,
    product: Arc<str>,
    context: Context,
}

impl QuantUnit {
    /// Describe one invocation with an authorized product label, stable identity and principal.
    /// Never pass a product label that the caller cannot read.
    /// # Errors
    /// `Programming` when the diagnostic label is empty.
    pub fn new(
        principal: impl Into<PrincipalHint>,
        identity: OperationIdentity,
        product: impl Into<Arc<str>>,
    ) -> Result<Self> {
        let product = product.into();
        if product.is_empty() {
            return Err(Error::programming("A stock unit requires a product label."));
        }
        Ok(Self {
            principal: principal.into(),
            identity,
            product,
            context: Context::new(),
        })
    }

    /// Preserve this entry-point context across attempts. The callback keeps its original caller.
    #[must_use]
    pub fn context(mut self, context: Context) -> Self {
        self.context = context;
        self
    }

    fn begin(&self) -> BeginOptions {
        BeginOptions::new(self.principal)
            .context(self.context.clone())
            .operation_identity(self.identity.clone())
            .on_primary()
    }
}

/// Stock contention exhausted its bounded whole-unit retry budget (`QuantRetryExhausted`,
/// .4). The source retains the last eligible error and its rollback diagnostics.
#[derive(Debug)]
#[non_exhaustive]
pub struct QuantRetryExhausted {
    product: Arc<str>,
    attempts: u32,
    cause: Error,
}

impl QuantRetryExhausted {
    /// The number of attempts that ran.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}
impl fmt::Display for QuantRetryExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Stock of {} is being changed by other operations. The operation was tried {} times and stopped; nothing was saved. Try again.",
            self.product, self.attempts
        )
    }
}
impl std::error::Error for QuantRetryExhausted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

struct Policy {
    lock: Duration,
    retry: RetryPolicy,
    idle: String,
    keepalive_idle: String,
    keepalive_interval: String,
    keepalive_count: String,
}

impl Policy {
    async fn load(orm: &Orm, unit: &QuantUnit) -> Result<Self> {
        let mut tx = orm
            .begin_opts(unit.begin().read_only("stock.quant_unit.policy"))
            .await?;
        let env = tx.env();
        let snapshot = read_values(
            &env,
            &mut tx,
            SettingsScope::Global {},
            QUANT_KEYS.iter().map(|key| (*key).to_owned()).collect(),
        )
        .await?;
        // Settings validates declared domains, groups and access in one snapshot. Missing or
        // malformed cells are errors, not a second source of defaults.
        let mut values = [0_u64; 8];
        for (index, key) in QUANT_KEYS.iter().enumerate() {
            let entry = snapshot
                .values
                .iter()
                .find(|entry| entry.key == *key)
                .ok_or_else(|| Error::programming(format!("Stock unit policy omitted {key}.")))?;
            let SettingValue::Integer(value) = entry.value else {
                return Err(Error::validation(format!(
                    "{key} requires a positive whole number."
                )));
            };
            let value = u64::try_from(value).map_err(|_| {
                Error::validation(format!("{key} requires a positive whole number."))
            })?;
            if value == 0 {
                return Err(Error::validation(format!(
                    "{key} requires a positive whole number."
                )));
            }
            values[index] = value;
        }
        let attempts = u32::try_from(values[1])
            .map_err(|_| Error::validation("stock.quant_max_attempts is outside its bound."))?;
        let policy = Self {
            lock: Duration::from_millis(values[0]),
            retry: RetryPolicy::new(
                attempts,
                Duration::from_millis(values[2]),
                Duration::from_millis(values[3]),
            )?,
            idle: values[4].to_string(),
            keepalive_idle: values[5].to_string(),
            keepalive_interval: values[6].to_string(),
            keepalive_count: values[7].to_string(),
        };
        tx.commit().await?;
        drop(tx);
        Ok(policy)
    }

    async fn arm(&self, tx: &mut Transaction) -> Result<()> {
        // Values are validated decimal integers; parameter binding keeps them data. The true
        // argument scopes every setting to this transaction, including on pooled connections.
        tx.query("SELECT set_config('idle_in_transaction_session_timeout',$1,true), set_config('tcp_keepalives_idle',$2,true), set_config('tcp_keepalives_interval',$3,true), set_config('tcp_keepalives_count',$4,true)",
            &[&self.idle, &self.keepalive_idle, &self.keepalive_interval, &self.keepalive_count]).await?;
        Ok(())
    }
}

/// Run a complete repeatable stock unit with a single policy snapshot and stable identity.
/// Include the probe, document/line changes and quant engine call in `work`; do not commit or
/// perform external effects inside it. Register after-commit effects on the transaction.
/// This top-level native entry refuses an enclosing ORM/RPC unit. It does not implement durable
/// idempotency and must not run the separate, non-retrying negative-stock policy unit.
/// # Errors
/// Settings/access/validation failures propagate. Eligible contention retries the whole callback,
/// then returns a Lock error sourced by [`QuantRetryExhausted`]. Uncertain commit outcomes and
/// all non-contention errors remain terminal and preserve their original kind and diagnostic.
pub async fn run_quant_unit<T, F>(orm: &Orm, unit: QuantUnit, mut work: F) -> Result<T>
where
    F: AsyncFnMut(&mut Transaction) -> Result<T>,
{
    require_top_level_unit(ENTRY)?;
    let policy = Policy::load(orm, &unit).await?;
    let options = RunOptions::new(unit.begin(), policy.retry)
        .lock_timeout(policy.lock)
        .retry_lock_timeouts()
        .top_level(ENTRY);
    match orm
        .run_with_options(options, async |tx| {
            policy.arm(tx).await?;
            work(tx).await
        })
        .await
    {
        Ok(value) => Ok(value),
        Err(failure) if failure.exhausted() => {
            let exhausted = QuantRetryExhausted {
                product: unit.product,
                attempts: failure.attempts(),
                cause: failure.into_error(),
            };
            Err(Error::new(ErrorKind::Lock, exhausted.to_string()).with_source(exhausted))
        }
        Err(failure) => Err(failure.into_error()),
    }
}
