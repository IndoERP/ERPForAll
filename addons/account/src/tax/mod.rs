// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tax definition foundation. Calculation, children and reference probes are separate slices.
mod company;
pub(crate) use company::reject_effective as reject_effective_company_defaults;
mod defaults;
mod group;
mod lifecycle;
mod product;
mod repartition;
pub mod settings;
mod uniqueness;
mod validation;

use erp_sdk::Operation;
use erp_sdk::ValueMap;
use erp_sdk::prelude::*;
pub use group::AccountTaxGroup;
pub use repartition::AccountTaxRepartitionLine;

/// The refusal of a duplicate tax name (K2), our own wording. The index message in the
/// model attribute below repeats it word for word; the pre-check adds one line per duplicate.
pub(crate) const DUPLICATE_NAME: &str = "Each active tax name can be used once per company tree, tax type, tax scope and country: rename the tax or archive the existing one.";
/// The fields of the uniqueness key and its predicate (K3): a write that names none of
/// them cannot create a duplicate, so the pre-check does not run.
const KEY_FIELDS: [&str; 6] = [
    "name",
    "company_id",
    "type_tax_use",
    "tax_scope",
    "country_id",
    "active",
];

/// A company-owned tax definition. Monetary calculations are supplied by the shared tax kernel.
#[model(name = "account.tax", description = "Tax", order = "sequence, id", company_access = direct(field = "company_id", branch = "ancestor", empty = "deny"), company_match = "parent_of", unique_indexes = [("_active_source_name_uniq", "(root_company_id, (name ->> 'en_US'), type_tax_use, COALESCE(tax_scope, ''), country_id) WHERE active AND type_tax_use <> 'none'", "Each active tax name can be used once per company tree, tax type, tax scope and country: rename the tax or archive the existing one.")])]
pub struct AccountTax {
    /// Source-language name participates in uniqueness; translations remain ordinary labels.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,
    /// The tax's business use.
    #[field(selection, selection = [("sale", "Sales"), ("purchase", "Purchases"), ("none", "Group member")], default = "sale", required = true, tracking = true)]
    pub type_tax_use: Option<Arc<str>>,
    /// Optional product scope.
    #[field(selection, selection = [("service", "Services"), ("consu", "Goods")])]
    pub tax_scope: Option<Arc<str>>,
    /// Formula support arrives with the kernel-dependent formula slice.
    #[field(selection, selection = [("percent", "Percentage"), ("fixed", "Fixed"), ("division", "Percentage of price"), ("group", "Group of taxes")], default = "percent", required = true, tracking = true)]
    pub amount_type: Option<Arc<str>>,
    /// Archived taxes are retained but do not reserve the active uniqueness key.
    #[field(boolean, default = true, required = true)]
    pub active: bool,
    /// Owning company, proposed from the active company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        readonly = true,
        ondelete = "restrict",
        index = true
    )]
    pub company_id: Option<RecId>,
    /// Derived company-tree identity (K1), computed from `company_id`; never accepted
    /// from callers.
    #[field(
        many2one,
        comodel = "res.company",
        compute = "_compute_root_company",
        store = true,
        required = true,
        readonly = true,
        ondelete = "restrict"
    )]
    pub root_company_id: Option<RecId>,
    /// Definition order.
    #[field(integer, default = 1)]
    pub sequence: i32,
    /// Exact decimal configuration amount (not binary floating point).
    #[field(float, digits = (16,4), default = 0, required = true, tracking = true)]
    pub amount: Decimal,
    /// Sanitized rich description.
    #[field(html, translate = true)]
    pub description: Option<Arc<str>>,
    /// Translated invoice label.
    #[field(char, translate = true)]
    pub invoice_label: Option<Arc<str>>,
    /// Invoice label, falling back to the name.
    #[field(char, compute = "_compute_labels")]
    pub tax_label: Option<Arc<str>>,
    /// Whether prices include this tax.
    #[field(boolean, compute = "_compute_price_include")]
    pub price_include: bool,
    /// Company-owned default shown beside the override.
    #[field(selection, selection = [("tax_excluded", "Tax excluded"), ("tax_included", "Tax included")], related = "company_id.account_price_include")]
    pub company_price_include: Option<Arc<str>>,
    /// Empty means use the company default.
    #[field(selection, selection = [("tax_excluded", "Tax excluded"), ("tax_included", "Tax included")], tracking = true)]
    pub price_include_override: Option<Arc<str>>,
    /// Stored calculation flag; its application belongs to the tax kernel.
    #[field(boolean, default = false, tracking = true)]
    pub include_base_amount: bool,
    /// Whether an earlier tax may enlarge this tax's base.
    #[field(boolean, default = true, tracking = true)]
    pub is_base_affected: bool,
    /// Propagate analytic information to tax lines.
    #[field(boolean)]
    pub analytic: bool,
    /// Totals grouping.
    #[field(
        many2one,
        comodel = "account.tax.group",
        required = true,
        ondelete = "restrict"
    )]
    pub tax_group_id: Option<RecId>,
    /// Tax jurisdiction.
    #[field(
        many2one,
        comodel = "res.country",
        required = true,
        ondelete = "restrict"
    )]
    pub country_id: Option<RecId>,
    /// Jurisdiction code.
    #[field(char, related = "country_id.code")]
    pub country_code: Option<Arc<str>>,
    /// Canonical owned lines for both document kinds.
    #[field(
        one2many,
        comodel = "account.tax.repartition.line",
        inverse_name = "tax_id"
    )]
    pub repartition_line_ids: Vec<RecId>,
    /// Invoice distribution.
    #[field(
        one2many,
        comodel = "account.tax.repartition.line",
        inverse_name = "tax_id",
        domain = [("document_type", "=", "invoice")]
    )]
    pub invoice_repartition_line_ids: Vec<RecId>,
    /// Refund distribution.
    #[field(
        one2many,
        comodel = "account.tax.repartition.line",
        inverse_name = "tax_id",
        domain = [("document_type", "=", "refund")]
    )]
    pub refund_repartition_line_ids: Vec<RecId>,
    /// Rich legal notes, sanitized at the field boundary.
    #[field(html, translate = true)]
    pub invoice_legal_notes: Option<Arc<str>>,
    /// Whether the invoice tax distribution contains negative factors.
    #[field(boolean, compute = "_compute_distribution")]
    pub has_negative_factor: bool,
    /// Exact sum of invoice tax factors excluded from closing.
    #[field(float, digits = 0, compute = "_compute_distribution")]
    pub non_deductible_amount: Decimal,
    /// Referencing addons extend the probe before adding usage links.
    #[field(boolean, compute = "_compute_is_used")]
    pub is_used: bool,
    /// Internal row-version fence for concurrent distribution edits; not operator configuration.
    #[field(integer, readonly = true)]
    pub repartition_serial: i32,
}

#[methods(AccountTax, addon = "account")]
impl AccountTax {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountTax>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let requested = defaults::requested(&names, true);
        let values = sup.call(tx, this.as_records(), (requested,)).await?;
        defaults::complete(this.as_records(), tx, &names, values, true).await
    }
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountTax>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let records = this.as_records();
        admit(records, tx, &vals_list, Operation::Create).await?;
        for vals in &mut vals_list {
            reject_internal(vals)?;
            defaults::for_create(records, tx, vals, true).await?;
            company_defaults(records, tx, vals, true).await?;
            vals.insert("repartition_serial".into(), Cell::Int(0));
            repartition::defaults(vals);
            repartition::admit_size(vals, tx).await?;
        }
        uniqueness::check(records, tx, &vals_list, false).await?;
        let guard = lifecycle::Guard::enter(tx, &[]).await?;
        let created: Records = sup.call(tx, records, (vals_list,)).await?;
        guard.add(&created.ids())?;
        guard.validate(tx, records.env()).await?;
        Ok(created)
    }
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountTax>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        admit(records, tx, std::slice::from_ref(&vals), Operation::Write).await?;
        reject_internal(&vals)?;
        repartition::admit_size(&vals, tx).await?;
        let guard = lifecycle::Guard::enter(tx, &records.ids()).await?;
        let mut proposed = vals;
        // a new company moves only the derived root; country and tax group stay as they are
        if proposed.contains("company_id") {
            admit_company(records, tx, &mut proposed).await?;
        }
        if KEY_FIELDS.iter().any(|field| proposed.contains(field)) {
            let candidates = vec![proposed.clone(); records.len()];
            uniqueness::check(records, tx, &candidates, true).await?;
        }
        sup.call(tx, records, (proposed,)).await?;
        guard.validate(tx, records.env()).await
    }
    #[method(override)]
    async fn unlink(this: &RecordSet<AccountTax>, tx: &mut Transaction, sup: Super) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        let used: Vec<bool> = this.as_records().call(tx, "_is_used", ()).await?;
        if used.len() != this.len() {
            return Err(Error::programming(
                "Tax usage probe returned the wrong number of rows.",
            ));
        }
        if used.into_iter().any(|used| used) {
            return Err(Error::validation(
                "A tax that is in use cannot be deleted: archive it instead.",
            ));
        }
        let guard = lifecycle::Guard::enter(tx, &this.ids()).await?;
        guard.deleting(&this.ids())?;
        sup.call(tx, this.as_records(), ()).await
    }
    /// Extend this batch probe when introducing a relation that makes a tax used.
    #[method]
    async fn _is_used(this: &RecordSet<AccountTax>, _tx: &mut Transaction) -> Result<Vec<bool>> {
        Ok(vec![false; this.len()])
    }
    #[compute(depends = [])]
    async fn _compute_is_used(
        this: &RecordSet<AccountTax>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountTax>,
    ) -> Result<()> {
        let used: Vec<bool> = this.as_records().call(tx, "_is_used", ()).await?;
        if used.len() != this.len() {
            return Err(Error::programming(
                "Tax usage probe returned the wrong number of rows.",
            ));
        }
        for (i, used) in used.into_iter().enumerate() {
            out.set(i, AccountTax::IS_USED, used)?;
        }
        Ok(())
    }
    /// The first company of the tax company's parent path (K1). A company's parent never
    /// changes (`res.company` refuses `parent_id`), so `company_id` is the only dependency.
    #[compute(depends = ["company_id"], reads = ["company_id.parent_path"])]
    fn _compute_root_company(batch: &mut Batch<'_, AccountTax>) -> Result<()> {
        for i in batch.indices() {
            let root = match batch.get(i, AccountTax::COMPANY_ID)? {
                Some(company) => {
                    let path = batch.value_of(erp_addon_base::ResCompany::PARENT_PATH, company)?;
                    match path.as_deref().and_then(|p| p.split('/').next()) {
                        Some(first) if !first.is_empty() => Some(
                            first
                                .parse::<i64>()
                                .ok()
                                .and_then(RecId::from_real)
                                .ok_or_else(|| {
                                    Error::internal(format!(
                                        "company {} has the parent path {path:?}",
                                        company.get()
                                    ))
                                })?,
                        ),
                        _ => Some(company),
                    }
                }
                None => None,
            };
            batch.set(i, AccountTax::ROOT_COMPANY_ID, root)?;
        }
        Ok(())
    }
    #[compute(depends = ["name", "invoice_label"], depends_context = ["lang"])]
    async fn _compute_labels(
        this: &RecordSet<AccountTax>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountTax>,
    ) -> Result<()> {
        for (i, tax) in this.iter().enumerate() {
            let label = tax.get(tx, AccountTax::INVOICE_LABEL).await?;
            let label = match label {
                Some(s) if !s.is_empty() => Some(s),
                _ => tax.get(tx, AccountTax::NAME).await?,
            };
            out.set(i, AccountTax::TAX_LABEL, label)?;
        }
        Ok(())
    }
    #[compute(depends = ["price_include_override", "company_id.account_price_include"])]
    async fn _compute_price_include(
        this: &RecordSet<AccountTax>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountTax>,
    ) -> Result<()> {
        for (i, tax) in this.iter().enumerate() {
            let value = tax.get(tx, AccountTax::PRICE_INCLUDE_OVERRIDE).await?;
            let value = match value {
                Some(s) if !s.is_empty() => Some(s),
                _ => tax.get(tx, AccountTax::COMPANY_PRICE_INCLUDE).await?,
            };
            out.set(
                i,
                AccountTax::PRICE_INCLUDE,
                value.as_deref() == Some("tax_included"),
            )?;
        }
        Ok(())
    }
    #[compute(depends = ["repartition_line_ids.factor_percent", "repartition_line_ids.document_type", "repartition_line_ids.repartition_type", "repartition_line_ids.use_in_tax_closing"])]
    async fn _compute_distribution(
        this: &RecordSet<AccountTax>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountTax>,
    ) -> Result<()> {
        for (i, tax) in this.iter().enumerate() {
            let (negative, share) = validation::derived(tax.as_records(), tx).await?;
            out.set(i, AccountTax::HAS_NEGATIVE_FACTOR, negative)?;
            out.set(i, AccountTax::NON_DEDUCTIBLE_AMOUNT, share)?;
        }
        Ok(())
    }
    #[constrains("tax_group_id", "country_id")]
    async fn _check_group(this: &RecordSet<AccountTax>, tx: &mut Transaction) -> Result<()> {
        for tax in this.iter() {
            let group = tax.as_records().related(tx, "tax_group_id").await?;
            let country = group.cell(tx, "country_id").await?;
            if !country.is_null() && country != tax.as_records().cell(tx, "country_id").await? {
                return Err(Error::validation(
                    "The tax group belongs to another country than the tax: choose a tax group of the tax's country, or one without a country.",
                ));
            }
        }
        Ok(())
    }
    #[onchange("price_include_override", "price_include")]
    async fn _onchange_price(
        this: &RecordSet<AccountTax>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        if this.get(tx, AccountTax::PRICE_INCLUDE).await? {
            this.as_records()
                .write_base(
                    tx,
                    RawVals::new().set_cell("include_base_amount", Cell::Bool(true)),
                )
                .await?;
        }
        Ok(OnchangeResult::none())
    }
}

fn reject_internal(vals: &RawVals) -> Result<()> {
    if vals.contains("root_company_id") || vals.contains("repartition_serial") {
        return Err(Error::validation(
            "root_company_id and repartition_serial are derived tax fields and cannot be supplied.",
        ));
    }
    Ok(())
}

pub(crate) async fn admit(
    records: &Records,
    tx: &mut Transaction,
    vals: &[RawVals],
    operation: Operation,
) -> Result<()> {
    records.check_access(tx, operation).await?;
    for row in vals {
        for name in row
            .iter()
            .map(|(n, _)| n)
            .chain(row.iter_commands().map(|(n, _)| n))
        {
            if !tx
                .has_field_access(records.env(), records.spec().name, name, FieldOp::Write)
                .await?
            {
                return Err(Error::access(format!(
                    "Field {}.{name} cannot be written.",
                    records.spec().name
                )));
            }
        }
    }
    Ok(())
}

/// The given company, or the active one, admitted for the caller and set in `vals`.
async fn admit_company(model: &Records, tx: &mut Transaction, vals: &mut RawVals) -> Result<i64> {
    let company = match vals.get("company_id") {
        Some(Cell::Id(id)) if *id > 0 => *id,
        Some(other) => return Err(Error::validation(format!("Invalid tax company: {other:?}"))),
        None => model
            .env()
            .company_id()?
            .ok_or_else(|| Error::validation("Select a company before configuring taxes."))?,
    };
    if !model.env().is_superuser() {
        let active: Vec<i64> = model.env().companies()?.iter().map(|id| id.get()).collect();
        let allowed = tx.query("SELECT EXISTS (SELECT 1 FROM res_company owner JOIN res_company selected ON selected.parent_path LIKE owner.parent_path || '%' WHERE owner.id=$1 AND selected.id=ANY($2::int8[]))", &[&company, &active]).await?;
        let allowed: bool = allowed[0]
            .try_get(0)
            .map_err(|e| Error::db(format!("Tax company admission: {e}")))?;
        if !allowed {
            return Err(Error::access(
                "The tax company is outside the active company hierarchy.",
            ));
        }
    }
    vals.insert("company_id".into(), Cell::Id(company));
    model
        .env()
        .records("res.company")?
        .browse([company])?
        .check_access(tx, Operation::Read)
        .await?;
    Ok(company)
}

pub(crate) async fn company_defaults(
    model: &Records,
    tx: &mut Transaction,
    vals: &mut RawVals,
    tax: bool,
) -> Result<()> {
    let company = admit_company(model, tx, vals).await?;
    let company_record = model.env().records("res.company")?.browse([company])?;
    if !vals.contains("country_id") {
        let fiscal = company_record.cell(tx, "account_fiscal_country_id").await?;
        let country = if fiscal.is_null() {
            company_record
                .related(tx, "partner_id")
                .await?
                .cell(tx, "country_id")
                .await?
        } else {
            fiscal
        };
        vals.insert("country_id".into(), country);
    }
    if tax && !vals.contains("tax_group_id") {
        let groups = model.env().records("account.tax.group")?;
        for country in [
            vals.get("country_id").cloned().unwrap_or(Cell::Null),
            Cell::Null,
        ] {
            let value = match country {
                Cell::Id(id) => erp_sdk::domain::Value::from(id),
                Cell::Null => erp_sdk::domain::Value::from(false),
                _ => return Err(Error::validation("A tax needs a country.")),
            };
            let domain = Domain::and([
                Domain::condition("company_id", Operator::ParentOf, company)?,
                Domain::condition("country_id", Operator::Eq, value)?,
            ]);
            let found = groups
                .search_with(tx, &domain, &SearchOptions::new().limit(1))
                .await?;
            if let Some(id) = found.ids().first() {
                vals.insert("tax_group_id".into(), Cell::Id(*id));
                break;
            }
        }
    }
    Ok(())
}
