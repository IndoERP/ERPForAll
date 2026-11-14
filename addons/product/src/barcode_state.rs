// SPDX-License-Identifier: MIT OR Apache-2.0
//! Derived integrity state. No catalog defaults or ordinary-user ACLs.
use erp_sdk::prelude::*;

/// Permanent version fence; even an empty barcode scope keeps its row.
#[model(name = "product.barcode.scope", description = "Barcode Scope", log_access = false,
    constraints = [("_barcode_key", "UNIQUE (barcode)", "A barcode has one scope fence.")]) ]
pub struct BarcodeScope {
    /// Exact stored barcode, never normalized.
    #[field(char, required = true)]
    pub barcode: Option<Arc<str>>,
}

/// One physical holder. A template's single-variant mirror is represented by its variant only.
#[model(name = "product.barcode.claim", description = "Barcode Claim", log_access = false,
    constraints = [
        ("_scope_key", "UNIQUE NULLS NOT DISTINCT (barcode, company_id)", "A barcode is already assigned within this company scope."),
        ("_holder", "CHECK (num_nonnulls(variant_id, template_id, packaging_id) = 1)", "A barcode claim must have exactly one holder."),
        ("_variant", "UNIQUE (variant_id)", "A variant has one barcode claim."),
        ("_template", "UNIQUE (template_id)", "An independent template has one barcode claim."),
        ("_packaging", "UNIQUE (packaging_id)", "A packaging has one barcode claim.")
    ],
    company_access = unrestricted(reason = "Private integrity state; no ordinary-user ACLs"))]
pub struct BarcodeClaim {
    /// Exact stored barcode.
    #[field(char, required = true)]
    pub barcode: Option<Arc<str>>,
    /// Restrict deletion rather than silently widening a claim through SET NULL. Indexed: the
    /// unique key leads with the barcode, and a company delete checks this column.
    #[field(many2one, comodel = "res.company", ondelete = "restrict", index = true)]
    pub company_id: Option<RecId>,
    /// Variant owner; database cascades release claims too.
    #[field(many2one, comodel = "product.product", ondelete = "cascade")]
    pub variant_id: Option<RecId>,
    /// Independent template owner, excluding legitimate mirrors.
    #[field(many2one, comodel = "product.template", ondelete = "cascade")]
    pub template_id: Option<RecId>,
    /// Packaging owner, including deletion through its product or unit.
    #[field(many2one, comodel = "product.uom", ondelete = "cascade")]
    pub packaging_id: Option<RecId>,
}

fn managed_state_error() -> Error {
    Error::access(
        "Barcode integrity state is managed by product and packaging writes; direct changes are not allowed.",
    )
}

// Internal SQL reconciliation deliberately bypasses these methods. Neither a privileged ORM
// caller nor a data loader may delete a permanent fence or fabricate/re-scope a claim.
#[methods(BarcodeScope, addon = "product")]
impl BarcodeScope {
    #[method(override)]
    async fn create(
        _this: &RecordSet<BarcodeScope>,
        _tx: &mut Transaction,
        _vals_list: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        Err(managed_state_error())
    }
    #[method(override)]
    async fn write(
        _this: &RecordSet<BarcodeScope>,
        _tx: &mut Transaction,
        _vals: RawVals,
        _sup: Super,
    ) -> Result<()> {
        Err(managed_state_error())
    }
    #[method(override)]
    async fn unlink(
        _this: &RecordSet<BarcodeScope>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<()> {
        Err(managed_state_error())
    }
}

#[methods(BarcodeClaim, addon = "product")]
impl BarcodeClaim {
    #[method(override)]
    async fn create(
        _this: &RecordSet<BarcodeClaim>,
        _tx: &mut Transaction,
        _vals_list: Vec<RawVals>,
        _sup: Super,
    ) -> Result<Records> {
        Err(managed_state_error())
    }
    #[method(override)]
    async fn write(
        _this: &RecordSet<BarcodeClaim>,
        _tx: &mut Transaction,
        _vals: RawVals,
        _sup: Super,
    ) -> Result<()> {
        Err(managed_state_error())
    }
    #[method(override)]
    async fn unlink(
        _this: &RecordSet<BarcodeClaim>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<()> {
        Err(managed_state_error())
    }
}
