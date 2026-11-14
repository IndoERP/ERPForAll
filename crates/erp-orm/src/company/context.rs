//! The validated company read of a transaction (review S1/S4): the one place
//! where an environment's companies are checked against the newest resolution of the subject
//! they must belong to, whatever the elevation. Code that takes a decision from the active
//! companies (ir.default lookups, sequences, the company check, context-dependent cache keys)
//! reads them here and never from [`Env::companies`] alone, which only knows what the principal
//! was built with.

use erp_fields::{Error, Result};

use crate::principal::CompanyId;
use crate::{Env, Transaction};

impl Transaction {
    /// The active companies of `env`, current first, checked against the newest resolution of the
    /// subject they belong to in this transaction, also under `sudo()` and for
    /// an environment from `with_user`: a membership change made in this transaction is seen
    /// (the policy and the resolutions are brought up to date first), and a subject not resolved
    /// yet is read through the principal resolver (one statement, once per subject).
    ///
    /// Only a `System` principal the core created takes the list as given; one made by
    /// `with_user(SUPERUSER_ID)` from another principal is bounded by that principal's companies.
    /// Without a principal resolver (static test policies) an unresolved principal takes a
    /// well-formed list as given.
    ///
    /// ```no_run
    /// # async fn demo(tx: &mut erp_orm::Transaction, env: &erp_orm::Env) -> erp_fields::Result<()> {
    /// let companies = tx.companies(env).await?;
    /// let current = companies.first();
    /// # let _ = current; Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// `Access` ("One of the selected companies does not exist or is not one of yours.") when a company is not one of the
    /// subject's; `Validation` for a malformed or duplicated list; `Programming` for an environment
    /// outside its elevation block or widened; the resolver's and the policy
    /// refresh's errors.
    pub async fn companies(&mut self, env: &Env) -> Result<Vec<CompanyId>> {
        // an environment outside its elevation block decides nothing; the switchers
        // of its with_user switches are resolved first (A4)
        self.prepare_switch(env).await?;
        self.check_env_block(env)?;
        env.companies()?;
        self.prepare_access(env).await?;
        Ok(self.companies_now(env)?.to_vec())
    }

    /// [`Transaction::companies`] without awaiting: for code that cannot (cache keys). The
    /// subject must be resolved and the transaction current, which every read and write entry
    /// point ensures ([`Transaction::prepare_access`]).
    ///
    /// # Errors
    ///
    /// As [`Transaction::companies`]; `Programming` when the subject has not been resolved in
    /// this transaction (a resolver is installed) or a policy or membership change is pending.
    pub(crate) fn companies_now<'a>(&'a self, env: &'a Env) -> Result<&'a [CompanyId]> {
        let allowed = env.companies()?;
        let principal = env.principal();
        let Some(authority) = principal.company_authority() else {
            return Ok(allowed);
        };
        if self.refresh.stale() {
            return Err(Error::programming(
                "the access policy of this transaction is stale: a policy or membership change is \
                 pending and the companies were read without Transaction::companies",
            ));
        }
        let newest = match self.resolved(authority.subject()) {
            Some(newest) => newest,
            None if self.shared.resolver.is_some() => {
                return Err(Error::programming(format!(
                    "the companies of user {} were not validated by the principal resolver in \
                     this transaction: read them with Transaction::companies",
                    authority.subject()
                )));
            }
            None => return Ok(allowed),
        };
        if allowed.is_empty() {
            // an environment that names no companies (a principal not resolved yet, as from
            // `with_user`) acts with the subject's default companies (review R2)
            return Ok(newest.default_companies());
        }
        let all = newest.all_companies();
        if allowed.iter().any(|c| all.binary_search(c).is_err()) {
            return Err(Error::access(
                "One of the selected companies does not exist or is not one of yours.",
            ));
        }
        Ok(allowed)
    }
}
