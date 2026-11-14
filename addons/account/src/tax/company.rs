// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company-owned tax configuration; cash-basis and fiscal-position fields belong to their slices.
use erp_sdk::prelude::*;

#[extend(ResCompany, addon = "account")]
struct TaxCompany {
    /// Fiscal jurisdiction. Tax defaults use the company's country when this is empty.
    #[field(many2one, comodel = "res.country", string = "Fiscal country")]
    pub account_fiscal_country_id: Option<RecId>,
    /// Whether tax is included when a tax definition has no explicit override.
    #[field(selection, selection = [("tax_excluded", "Tax excluded"), ("tax_included", "Tax included")], default = "tax_excluded", required = true, string = "Default tax price inclusion")]
    pub account_price_include: Option<Arc<str>>,
    /// Editable sale-tax choice for products created in selected companies.
    #[field(
        many2one,
        comodel = "account.tax",
        check_company = true,
        ondelete = "restrict",
        string = "Default sales tax"
    )]
    pub account_sale_tax_id: Option<RecId>,
    /// Editable purchase-tax choice, independent of the sales choice.
    #[field(
        many2one,
        comodel = "account.tax",
        check_company = true,
        ondelete = "restrict",
        string = "Default tax on purchases"
    )]
    pub account_purchase_tax_id: Option<RecId>,
    /// Stored inherited sale default; readers never need to open an ancestor company row.
    #[field(
        many2one,
        comodel = "account.tax",
        compute = "_compute_effective_tax_defaults",
        store = true,
        readonly = true,
        check_company = true
    )]
    pub effective_account_sale_tax_id: Option<RecId>,
    /// Stored inherited purchase default, independent of sale.
    #[field(
        many2one,
        comodel = "account.tax",
        compute = "_compute_effective_tax_defaults",
        store = true,
        readonly = true,
        check_company = true
    )]
    pub effective_account_purchase_tax_id: Option<RecId>,
}

pub(crate) fn reject_effective(vals: &RawVals) -> Result<()> {
    if vals.contains("effective_account_sale_tax_id")
        || vals.contains("effective_account_purchase_tax_id")
    {
        return Err(Error::validation(
            "Effective company tax defaults are computed and cannot be assigned.",
        ));
    }
    Ok(())
}
#[methods(ResCompany, addon = "account")]
impl TaxCompany {
    #[compute(depends = ["account_sale_tax_id", "account_purchase_tax_id", "parent_id.effective_account_sale_tax_id", "parent_id.effective_account_purchase_tax_id"])]
    fn _compute_effective_tax_defaults(batch: &mut Batch<'_, ResCompany>) -> Result<()> {
        for i in batch.indices() {
            let parent = batch.get(i, ResCompany::PARENT_ID)?;
            let sale = match batch.get(i, TaxCompany::ACCOUNT_SALE_TAX_ID)? {
                own @ Some(_) => own,
                None => match parent {
                    Some(parent) => {
                        batch.value_of(TaxCompany::EFFECTIVE_ACCOUNT_SALE_TAX_ID, parent)?
                    }
                    None => None,
                },
            };
            let purchase = match batch.get(i, TaxCompany::ACCOUNT_PURCHASE_TAX_ID)? {
                own @ Some(_) => own,
                None => match parent {
                    Some(parent) => {
                        batch.value_of(TaxCompany::EFFECTIVE_ACCOUNT_PURCHASE_TAX_ID, parent)?
                    }
                    None => None,
                },
            };
            batch.set(i, TaxCompany::EFFECTIVE_ACCOUNT_SALE_TAX_ID, sale)?;
            batch.set(i, TaxCompany::EFFECTIVE_ACCOUNT_PURCHASE_TAX_ID, purchase)?;
        }
        Ok(())
    }
}
