// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fiscal-position setup and resolution. Document and pack consumers are separate.
mod account;
mod company;
mod country_group;
mod lifecycle;
mod mapping;
mod matcher;
mod partner;
mod tax;
mod validation;
use erp_sdk::{Operation, ValueMap, prelude::*};

/// A company-owned rule for selecting tax and account substitutions.
///
/// ```rust,no_run
/// # async fn example(tx: &mut erp_sdk::Transaction, company: i64, partner: i64, taxes: Vec<i64>) -> erp_sdk::Result<()> {
/// let positions = tx.env().with_company(company)?.records("account.fiscal.position")?;
/// let selected: Option<i64> = positions.call(tx, "_get_fiscal_position", (partner, None::<i64>)).await?;
/// let rule = positions.browse(selected)?;
/// let mapped: Vec<i64> = rule.call(tx, "map_tax", (taxes,)).await?;
/// # let _ = mapped;
/// # Ok(()) }
/// ```
#[model(name = "account.fiscal.position", description = "Fiscal position", order = "sequence, id", company_access = direct(field = "company_id", branch = "ancestor", empty = "deny"), company_match = "parent_of")]
pub struct FiscalPosition {
    /// Name shown when selecting this rule.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,
    /// Owning company; defaults to the current company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        ondelete = "restrict"
    )]
    pub company_id: Option<RecId>,
    /// Administrator-owned ordering within a company.
    #[field(integer, default = 0)]
    pub sequence: i32,
    /// Archived rules remain available for historical references.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Include this rule in automatic resolution.
    #[field(boolean, default = false)]
    pub auto_apply: bool,
    /// Require the selected address to have a tax identifier.
    #[field(boolean, default = false)]
    pub vat_required: bool,
    /// Optional country predicate.
    #[field(many2one, comodel = "res.country")]
    pub country_id: Option<RecId>,
    /// Optional group predicate, including its fiscal exceptions.
    #[field(many2one, comodel = "res.country.group")]
    pub country_group_id: Option<RecId>,
    /// Allowed states; an empty set does not restrict matching.
    #[field(many2many, comodel = "res.country.state")]
    pub state_ids: Vec<RecId>,
    /// Inclusive lexical ZIP lower bound.
    #[field(char)]
    pub zip_from: Option<Arc<str>>,
    /// Inclusive lexical ZIP upper bound.
    #[field(char)]
    pub zip_to: Option<Arc<str>>,
    /// Normalized identifier; national validation is a later base capability.
    #[field(char)]
    pub foreign_vat: Option<Arc<str>>,
    /// Sanitized legal notes, translatable by the administrator.
    #[field(html, translate = true)]
    pub note: Option<Arc<str>>,
    /// Taxes offered by this position, inverse of the tax-side relation.
    #[field(
        many2many,
        comodel = "account.tax",
        relation = "account_fiscal_position_account_tax_rel",
        column1 = "account_fiscal_position_id",
        column2 = "account_tax_id"
    )]
    pub tax_ids: Vec<RecId>,
    /// Explicit account substitutions.
    #[field(
        one2many,
        comodel = "account.fiscal.position.account",
        inverse_name = "position_id"
    )]
    pub account_ids: Vec<RecId>,
    /// Derived source-tax ids to destination-tax ids.
    #[field(json, compute = "_compute_tax_map")]
    pub tax_map: Option<erp_sdk::serde_json::Value>,
    /// Derived source-account ids to destination-account ids.
    #[field(json, compute = "_compute_account_map")]
    pub account_map: Option<erp_sdk::serde_json::Value>,
    /// Whether this is the owning company's selected domestic rule.
    #[field(boolean, compute = "_compute_is_domestic")]
    pub is_domestic: bool,
    /// The company's fiscal jurisdiction.
    #[field(
        many2one,
        comodel = "res.country",
        related = "company_id.account_fiscal_country_id"
    )]
    pub company_country_id: Option<RecId>,
    /// Number of states in the selected country.
    #[field(integer, compute = "_compute_states_count")]
    pub states_count: i32,
}
#[methods(FiscalPosition, addon = "account")]
impl FiscalPosition {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<FiscalPosition>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut vals: ValueMap = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "company_id")
            && !vals.contains_key("company_id")
            && let Some(company) = this.env().company_id()?
        {
            vals.insert("company_id".into(), Value::Int(company));
        }
        Ok(vals)
    }
    #[method(override)]
    async fn create(
        this: &RecordSet<FiscalPosition>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let records = this.as_records();
        records.check_access(tx, Operation::Create).await?;
        let mut prepared = Vec::with_capacity(vals_list.len());
        for vals in vals_list {
            prepared.push(validation::prepare(records, tx, vals, None).await?);
        }
        validation::fence(records, tx, &prepared).await?;
        let created: Records = sup.call(tx, records, (prepared,)).await?;
        validation::validate(&created, tx).await?;
        Ok(created)
    }
    #[method(override)]
    async fn write(
        this: &RecordSet<FiscalPosition>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        let mut prepared = Vec::with_capacity(records.len());
        for record in records.iter() {
            prepared.push(validation::prepare(records, tx, vals.clone(), Some(&record)).await?);
        }
        validation::fence(records, tx, &prepared).await?;
        let guard = lifecycle::Guard::enter(tx, &records.ids())?;
        if records.len() > 1 {
            // Re-enter ordinary dispatch for each independently normalized row, retaining
            // all addon overrides. The roots above are fenced in stable order first.
            for record in records.iter() {
                record.write(tx, vals.clone()).await?;
            }
        } else if let Some(vals) = prepared.pop() {
            sup.call(tx, records, (vals,)).await?;
        } else {
            return Ok(());
        }
        guard.validate(tx, records).await
    }
    /// Resolve an address under the current company. Returns no id when no rule matches.
    /// Errors propagate from access checks, company context and addon predicates.
    #[method]
    async fn _get_fiscal_position(
        this: &RecordSet<FiscalPosition>,
        tx: &mut Transaction,
        partner_id: i64,
        delivery_id: Option<i64>,
    ) -> Result<Option<i64>> {
        matcher::resolve(this.as_records(), tx, partner_id, delivery_id).await
    }
    /// Conjunctive candidate predicate. Addons extend it through ordinary Super dispatch.
    #[method]
    async fn _fpos_matches(
        this: &RecordSet<FiscalPosition>,
        tx: &mut Transaction,
        partner_id: i64,
    ) -> Result<bool> {
        matcher::matches(this.ensure_one()?.as_records(), tx, partner_id).await
    }
    /// Substitute taxes in input order, deduplicating destinations. An empty position is identity.
    #[method]
    async fn map_tax(
        this: &RecordSet<FiscalPosition>,
        tx: &mut Transaction,
        tax_ids: Vec<i64>,
    ) -> Result<Vec<i64>> {
        mapping::taxes(this.as_records(), tx, tax_ids).await
    }
    /// Resolve a configured account substitution, retaining an unmapped account.
    #[method]
    async fn map_account(
        this: &RecordSet<FiscalPosition>,
        tx: &mut Transaction,
        account_id: i64,
    ) -> Result<i64> {
        mapping::account(this.as_records(), tx, account_id).await
    }
    #[compute(depends = ["tax_ids.original_tax_ids", "tax_ids.active"])]
    async fn _compute_tax_map(
        this: &RecordSet<FiscalPosition>,
        tx: &mut Transaction,
        out: &mut Assign<'_, FiscalPosition>,
    ) -> Result<()> {
        for (i, rec) in this.iter().enumerate() {
            out.set(
                i,
                FiscalPosition::TAX_MAP,
                Some(mapping::tax_map(rec.as_records(), tx).await?),
            )?;
        }
        Ok(())
    }
    #[compute(depends = ["account_ids.account_src_id", "account_ids.account_dest_id"])]
    async fn _compute_account_map(
        this: &RecordSet<FiscalPosition>,
        tx: &mut Transaction,
        out: &mut Assign<'_, FiscalPosition>,
    ) -> Result<()> {
        for (i, rec) in this.iter().enumerate() {
            out.set(
                i,
                FiscalPosition::ACCOUNT_MAP,
                Some(mapping::account_map(rec.as_records(), tx).await?),
            )?;
        }
        Ok(())
    }
    #[compute(depends = ["company_id.domestic_fiscal_position_id"])]
    async fn _compute_is_domestic(
        this: &RecordSet<FiscalPosition>,
        tx: &mut Transaction,
        out: &mut Assign<'_, FiscalPosition>,
    ) -> Result<()> {
        for (i, rec) in this.iter().enumerate() {
            let company = rec.as_records().related(tx, "company_id").await?;
            out.set(
                i,
                FiscalPosition::IS_DOMESTIC,
                company.cell(tx, "domestic_fiscal_position_id").await? == Cell::Id(rec.ids()[0]),
            )?;
        }
        Ok(())
    }
    #[compute(depends = ["country_id.state_ids"])]
    async fn _compute_states_count(
        this: &RecordSet<FiscalPosition>,
        tx: &mut Transaction,
        out: &mut Assign<'_, FiscalPosition>,
    ) -> Result<()> {
        for (i, rec) in this.iter().enumerate() {
            let country = rec.as_records().related(tx, "country_id").await?;
            let states = country.related(tx, "state_ids").await?;
            out.set(
                i,
                FiscalPosition::STATES_COUNT,
                i32::try_from(states.len())
                    .map_err(|_| Error::validation("Too many states for states_count."))?,
            )?;
        }
        Ok(())
    }
}

pub(super) fn id(cell: Cell) -> Result<Option<i64>> {
    match cell {
        Cell::Null => Ok(None),
        Cell::Id(id) if id > 0 => Ok(Some(id)),
        other => Err(Error::programming(format!(
            "Expected optional record id, got {other:?}"
        ))),
    }
}
pub(super) fn text(cell: Cell) -> Result<String> {
    match cell {
        Cell::Null => Ok(String::new()),
        Cell::Str(s) => Ok(s.to_string()),
        other => Err(Error::programming(format!(
            "Expected optional text, got {other:?}"
        ))),
    }
}
