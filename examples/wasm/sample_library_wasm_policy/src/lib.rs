//! Sample addon `sample_library_wasm_policy`: a second WebAssembly addon, installed on
//! top of `sample_library_wasm` from its own package.
//!
//! It overrides `renew` of `sample_library.loan` and calls the layer below with `super` once the
//! rule allows it. A refused renewal is a validation error: the caller sees the message and
//! nothing is written.

use erp_addon_sample_library_wasm::{Loan, LoanRenewals};
use erp_sdk::prelude::*;

erp_sdk::addon!(
    name = "sample_library_wasm_policy",
    depends = ["sample_library_wasm"]
);

// the leaf crate exports the component; natively this expands to nothing
erp_sdk::export_component!();

/// How often a loan may be renewed.
pub const MAX_RENEWALS: i32 = 2;

/// This addon's field on a loan.
#[extend(Loan)]
pub struct RenewalPolicy {
    /// A locked loan is never renewed.
    #[field(boolean, string = "Renewal locked")]
    pub renewal_locked: bool,
}

#[methods(Loan)]
impl RenewalPolicy {
    /// Refuse a locked loan and a loan that reached the limit; otherwise renew through the layer
    /// below.
    #[method(override, wire)]
    async fn renew(
        this: &RecordSet<Loan>,
        tx: &mut Transaction,
        note: Option<String>,
        sup: Super,
    ) -> Result<i32> {
        let loan = this.ensure_one()?;
        if loan.get(tx, RenewalPolicy::RENEWAL_LOCKED).await? {
            return Err(Error::validation(
                "This loan is locked and cannot be renewed.",
            ));
        }
        if loan.get(tx, LoanRenewals::RENEWALS).await? >= MAX_RENEWALS {
            return Err(Error::validation(format!(
                "A loan can be renewed at most {MAX_RENEWALS} times."
            )));
        }
        sup.call(tx, this, (note,)).await
    }
}
