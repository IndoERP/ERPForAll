// SPDX-License-Identifier: MIT OR Apache-2.0
//! A provider exchange has its own settled charge. The next reservation replaces the
//! previous admission lease atomically, so concurrent turns cannot bypass either budget.
use erp_addon_ai_base::{ledger, routing::ConfiguredRoute};
use erp_fields::{Error, Result};
use erp_orm::{CompanyId, Context, CredentialOptions, Principal, Transaction};
use erp_sdk::ai::{ToolTurnRequest, UsageRecord};
use erp_security::auth::{Auth, SessionSecret};

pub(crate) fn estimate(route: &ConfiguredRoute, request: &ToolTurnRequest) -> Result<i64> {
    let encoded = route
        .request_encoder()
        .and_then(|encoder| encoder.encode(request))
        .map_err(|_| {
            Error::validation("erp.ai.message_invalid: message exceeds the provider request limit.")
        })?;
    let bytes = serde_json::to_vec(&encoded)
        .map_err(|_| Error::internal("Could not encode the provider request."))?;
    i64::try_from(bytes.len())
        .map_err(|_| Error::validation("erp.ai.message_invalid: message is too large."))
}

#[derive(Clone)]
pub(crate) struct Authority {
    principal: Principal,
    policy: erp_orm::PolicyGeneration,
}

impl Authority {
    pub(crate) fn capture(tx: &Transaction) -> Self {
        Self {
            principal: tx.env().principal().clone(),
            policy: tx.policy().generation(),
        }
    }

    pub(crate) fn check(&self, tx: &Transaction) -> Result<()> {
        if tx.env().principal() != &self.principal || tx.policy().generation() != self.policy {
            return Err(Error::access(
                "erp.ai.authority_changed: your access changed during this turn. Reopen the conversation before continuing.",
            ));
        }
        Ok(())
    }
}

pub(crate) struct Meter<'a> {
    pub auth: &'a Auth,
    pub secret: &'a SessionSecret,
    pub company: CompanyId,
    pub authority: &'a Authority,
    pub scope: &'a str,
    pub route: &'a ConfiguredRoute,
    pub expiry: i32,
    pub reserved: ledger::Reservation,
    pub usage: Option<(i64, i64)>,
}
impl Meter<'_> {
    pub fn record(&mut self, usage: &UsageRecord) {
        self.usage = Some((usage.prompt_tokens.into(), usage.completion_tokens.into()));
    }

    pub async fn next(&mut self, request: &ToolTurnRequest) -> Result<()> {
        let input = estimate(self.route, request)?;
        let output = request
            .max_tokens()
            .ok_or_else(|| Error::invariant("Unbounded assistant request."))?;
        let opts =
            CredentialOptions::new().context(Context::new().with_company_id(self.company.get()));
        let mut tx = self.auth.begin_session(self.secret, opts).await?.tx;
        let env = tx.env();
        self.authority.check(&tx)?;
        if !tx.has_group(&env, "base.group_user").await?
            || env.is_superuser()
            || env.principal().actor().is_some()
        {
            return Err(Error::access("An interactive session is required."));
        }
        self.reserved.settle_active(&mut tx, self.usage).await?;
        let reserved =
            ledger::reserve(&mut tx, self.scope, self.route, input, output, self.expiry).await?;
        tx.commit().await?;
        self.reserved = reserved;
        self.usage = None;
        Ok(())
    }
}
