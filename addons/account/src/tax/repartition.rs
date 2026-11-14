// SPDX-License-Identifier: MIT OR Apache-2.0
//! Owned tax distributions, with parent-coordinated mutations.
use super::lifecycle::Guard;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

/// One component of an invoice or refund tax distribution.
#[model(name = "account.tax.repartition.line", description = "Tax repartition line", order = "document_type, repartition_type, sequence, id", company_access = direct(field = "company_id", branch = "ancestor", empty = "allow", parent = "tax_id"), company_match = "parent_of")]
pub struct AccountTaxRepartitionLine {
    /// Exact percentage, separate from presentation precision.
    #[field(float, digits = (16,12), default = 100, required = true)]
    pub factor_percent: Decimal,
    /// Exact ratio of the percentage to one hundred.
    #[field(float, digits = 0, compute = "_compute_factor")]
    pub factor: Decimal,
    /// Whether this distributes the taxable base or the tax amount.
    #[field(selection, selection = [("base", "Base"),("tax", "Tax")], default = "tax", required = true)]
    pub repartition_type: Option<Arc<str>>,
    /// Document side of the definition.
    #[field(selection, selection = [("invoice", "Invoice"),("refund", "Refund")], required = true)]
    pub document_type: Option<Arc<str>>,
    /// Optional posting account.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        index = "btree_not_null",
        domain = [("account_type", "not in", ["asset_receivable", "liability_payable", "off_balance"])]
    )]
    pub account_id: Option<RecId>,
    /// Posting account archive state.
    #[field(boolean, related = "account_id.active")]
    pub account_active: bool,
    /// Reporting-grid tags, restricted from deletion while referenced.
    #[field(many2many, comodel = "account.account.tag", ondelete = "restrict", domain = [("applicability", "=", "taxes")])]
    pub tag_ids: Vec<RecId>,
    /// Domestic tag-selection domain. Foreign VAT jurisdictions are added.
    #[field(json, compute = "_compute_tag_domain")]
    pub tag_ids_domain: Option<erp_sdk::serde_json::Value>,
    /// Required owning tax (A1-D4).
    #[field(
        many2one,
        comodel = "account.tax",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub tax_id: Option<RecId>,
    /// Company follows the owning tax and cannot be reassigned independently.
    #[field(
        many2one,
        comodel = "res.company",
        related = "tax_id.company_id",
        store = true
    )]
    pub company_id: Option<RecId>,
    /// Stable ordering within each distribution.
    #[field(integer, default = 1)]
    pub sequence: i32,
    /// Whether this share enters the tax closing entry.
    #[field(boolean, compute = "_compute_closing", store = true, readonly = false)]
    pub use_in_tax_closing: bool,
}
#[methods(AccountTaxRepartitionLine, addon = "account")]
impl AccountTaxRepartitionLine {
    #[compute(depends = ["factor_percent"])]
    async fn _compute_factor(
        this: &RecordSet<AccountTaxRepartitionLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountTaxRepartitionLine>,
    ) -> Result<()> {
        for (i, line) in this.iter().enumerate() {
            let factor = line
                .get(tx, AccountTaxRepartitionLine::FACTOR_PERCENT)
                .await?
                .checked_div(Decimal::from(100))
                .ok_or_else(|| Error::validation("Tax repartition decimal overflow."))?;
            out.set(i, AccountTaxRepartitionLine::FACTOR, factor)?;
        }
        Ok(())
    }
    #[compute(depends = ["account_id.internal_group", "repartition_type"])]
    async fn _compute_closing(
        this: &RecordSet<AccountTaxRepartitionLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountTaxRepartitionLine>,
    ) -> Result<()> {
        for (i, line) in this.iter().enumerate() {
            let account = line.as_records().related(tx, "account_id").await?;
            let mut closing = false;
            if line
                .get(tx, AccountTaxRepartitionLine::REPARTITION_TYPE)
                .await?
                .as_deref()
                == Some("tax")
                && !account.is_empty()
            {
                let group = account.cell(tx, "internal_group").await?;
                closing =
                    matches!(group,Cell::Str(ref s) if !matches!(s.as_ref(),"income"|"expense"));
            }
            out.set(i, AccountTaxRepartitionLine::USE_IN_TAX_CLOSING, closing)?;
        }
        Ok(())
    }
    #[compute(depends = ["company_id.account_fiscal_country_id", "company_id.partner_id.country_id"])]
    async fn _compute_tag_domain(
        this: &RecordSet<AccountTaxRepartitionLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountTaxRepartitionLine>,
    ) -> Result<()> {
        for (index, line) in this.iter().enumerate() {
            let company = line.as_records().related(tx, "company_id").await?;
            let fiscal = company.cell(tx, "account_fiscal_country_id").await?;
            let fiscal = if fiscal.is_null() {
                company
                    .related(tx, "partner_id")
                    .await?
                    .cell(tx, "country_id")
                    .await?
            } else {
                fiscal
            };
            let mut countries = vec![erp_sdk::serde_json::Value::Bool(false)];
            match fiscal {
                Cell::Id(id) => countries.push(erp_sdk::serde_json::json!(id)),
                Cell::Null => {}
                other => {
                    return Err(Error::programming(format!(
                        "Invalid fiscal country: {other:?}"
                    )));
                }
            }
            out.set(
                index,
                AccountTaxRepartitionLine::TAG_IDS_DOMAIN,
                Some(erp_sdk::serde_json::json!([
                    ["applicability", "=", "taxes"],
                    ["country_id", "in", countries]
                ])),
            )?;
        }
        Ok(())
    }
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountTaxRepartitionLine>,
        tx: &mut Transaction,
        vals: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        super::admit(this.as_records(), tx, &vals, Operation::Create).await?;
        let mut ids = Vec::new();
        for row in &vals {
            if row.contains("company_id") {
                return Err(Error::validation(
                    "A repartition line's company is derived from its tax.",
                ));
            }
            if let Some(Cell::Id(id)) = row.get("tax_id") {
                ids.push(*id);
            } else {
                return Err(Error::validation(
                    "A tax repartition line requires an owning tax.",
                ));
            }
        }
        owners(this.env(), tx, &ids).await?;
        let guard = Guard::enter(tx, &ids).await?;
        let created: Records = sup.call(tx, this.as_records(), (vals,)).await?;
        guard.validate(tx, this.env()).await?;
        Ok(created)
    }
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountTaxRepartitionLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        super::admit(
            this.as_records(),
            tx,
            std::slice::from_ref(&vals),
            Operation::Write,
        )
        .await?;
        let mut ids = this.as_records().related(tx, "tax_id").await?.ids();
        if let Some(Cell::Id(id)) = vals.get("tax_id") {
            ids.push(*id);
        }
        owners(this.env(), tx, &ids).await?;
        let guard = Guard::enter(tx, &ids).await?;
        if vals.contains("company_id") {
            return Err(Error::validation(
                "A repartition line's company is derived from its tax.",
            ));
        }
        sup.call(tx, this.as_records(), (vals,)).await?;
        guard.validate(tx, this.env()).await
    }
    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountTaxRepartitionLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        let ids = this.as_records().related(tx, "tax_id").await?.ids();
        owners(this.env(), tx, &ids).await?;
        let guard = Guard::enter(tx, &ids).await?;
        sup.call(tx, this.as_records(), ()).await?;
        guard.validate(tx, this.env()).await
    }
    #[onchange("repartition_type")]
    async fn _onchange_type(
        this: &RecordSet<AccountTaxRepartitionLine>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        if this
            .get(tx, AccountTaxRepartitionLine::REPARTITION_TYPE)
            .await?
            .as_deref()
            == Some("base")
        {
            this.as_records()
                .write_base(tx, RawVals::new().set_cell("account_id", Cell::Null))
                .await?;
        }
        Ok(OnchangeResult::none())
    }
}
async fn owners(env: &Env, tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    env.records("account.tax")?
        .browse(ids.iter().copied())?
        .check_access(tx, Operation::Write)
        .await
}
pub(crate) fn defaults(vals: &mut RawVals) {
    if vals.commands("repartition_line_ids").is_some() || vals.contains("repartition_line_ids") {
        return;
    }
    for (field, doc) in [
        ("invoice_repartition_line_ids", "invoice"),
        ("refund_repartition_line_ids", "refund"),
    ] {
        if vals.commands(field).is_some() || vals.contains(field) {
            continue;
        }
        vals.insert_commands(field.into(), default_lines(doc, &RawVals::new()));
    }
}

/// The default distribution of one document kind, one base and one tax line (the reference's
/// `_compute_invoice_repartition_line_ids`), each carrying `line` as further proposed values.
pub(crate) fn default_lines(document: &str, line: &RawVals) -> Vec<Command> {
    ["base", "tax"]
        .into_iter()
        .map(|kind| {
            Command::create(
                line.clone()
                    .set_cell("document_type", Cell::Str(document.into()))
                    .set_cell("repartition_type", Cell::Str(kind.into())),
            )
        })
        .collect()
}

// Refuse oversized creation commands before the ORM materializes child rows. Complete final-state
// validation still runs after the transaction-local parent/child scope closes.
pub(crate) async fn admit_size(vals: &RawVals, tx: &mut Transaction) -> Result<()> {
    let limit = super::settings::limit(tx).await?;
    let mut counts = [0usize; 2];
    for (field, commands) in vals.iter_commands() {
        if !matches!(
            field,
            "repartition_line_ids" | "invoice_repartition_line_ids" | "refund_repartition_line_ids"
        ) {
            continue;
        }
        for command in commands {
            if let Command::Create(row) = command {
                let document = match row.get("document_type") {
                    Some(Cell::Str(s)) => s.as_ref(),
                    None if field == "invoice_repartition_line_ids" => "invoice",
                    None if field == "refund_repartition_line_ids" => "refund",
                    _ => {
                        return Err(Error::validation(
                            "A tax repartition line needs invoice or refund as its document type.",
                        ));
                    }
                };
                let slot = match document {
                    "invoice" => 0,
                    "refund" => 1,
                    _ => {
                        return Err(Error::validation(
                            "A tax repartition line needs invoice or refund as its document type.",
                        ));
                    }
                };
                counts[slot] = counts[slot]
                    .checked_add(1)
                    .ok_or_else(|| Error::validation("Tax repartition line count overflow."))?;
                if counts[slot] > limit {
                    return Err(Error::validation(format!(
                        "Tax distribution exceeds {limit} lines per document kind (setting {}).",
                        super::settings::REPARTITION_LINES_MAX
                    )));
                }
            }
        }
    }
    Ok(())
}
