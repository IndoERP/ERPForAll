//! `res.company`: company model.

use erp_sdk::prelude::*;
use erp_sdk::{Operation, ValueMap};

/// `res.company`.
#[model(
    name = "res.company",
    description = "Company",
    parent_store = true,
    parent_name = "parent_id",
    signal_triggers = [("_membership_bump", "membership", update_of = ["active"])],
)]
pub struct ResCompany {
    /// Company name.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Parent company.
    #[field(many2one, comodel = "res.company", ondelete = "restrict", index = true)]
    pub parent_id: Option<RecId>,
    /// Active companies participate in normal lists and company membership.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Active direct branches.
    #[field(one2many, comodel = "res.company", inverse_name = "parent_id")]
    pub child_ids: Vec<RecId>,
    /// Direct branches, including archived companies.
    #[field(one2many, comodel = "res.company", inverse_name = "parent_id", context = [("active_test", false)])]
    pub all_child_ids: Vec<RecId>,
    /// Materialized path of the company hierarchy.
    #[field(char, index = true)]
    pub parent_path: Option<Arc<str>>,
    /// Partner representing this company.
    #[field(many2one, comodel = "res.partner", ondelete = "restrict")]
    pub partner_id: Option<RecId>,
    /// Company currency.
    #[field(many2one, comodel = "res.currency", required = true)]
    pub currency_id: Option<RecId>,
    /// Street line 1 of the company contact (the partner's).
    #[field(
        char,
        related = "partner_id.street",
        readonly = false,
        compute_sudo = true
    )]
    pub street: Option<Arc<str>>,
    /// Street line 2 of the company contact.
    #[field(
        char,
        related = "partner_id.street2",
        readonly = false,
        compute_sudo = true
    )]
    pub street2: Option<Arc<str>>,
    /// Postal code of the company contact.
    #[field(
        char,
        related = "partner_id.zip",
        readonly = false,
        compute_sudo = true
    )]
    pub zip: Option<Arc<str>>,
    /// City of the company contact.
    #[field(
        char,
        related = "partner_id.city",
        readonly = false,
        compute_sudo = true
    )]
    pub city: Option<Arc<str>>,
    /// Email of the company contact.
    #[field(
        char,
        related = "partner_id.email",
        readonly = false,
        compute_sudo = true
    )]
    pub email: Option<Arc<str>>,
    /// Phone of the company contact.
    #[field(
        char,
        related = "partner_id.phone",
        readonly = false,
        compute_sudo = true
    )]
    pub phone: Option<Arc<str>>,
    /// Website of the company contact.
    #[field(
        char,
        related = "partner_id.website",
        readonly = false,
        compute_sudo = true
    )]
    pub website: Option<Arc<str>>,
    /// Tax ID of the company contact.
    #[field(
        char,
        related = "partner_id.vat",
        string = "Tax ID",
        readonly = false,
        compute_sudo = true
    )]
    pub vat: Option<Arc<str>>,
}

// Without it a related field is company-bound
// and refuses a company partner whose `company_id` lies outside the reader's active companies
// .

const PARTNER_MIRRORED: [&str; 8] = [
    "street", "street2", "zip", "city", "email", "phone", "website", "vat",
];

#[methods(ResCompany, addon = "base")]
impl ResCompany {
    /// The partner is created in the caller's environment with no default parent.
    #[method(override)]
    async fn create(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let records = this.as_records();
        records.check_access(tx, Operation::Create).await?;
        if vals_list.len() > 4096 {
            return Err(Error::validation(
                "Company lifecycle maximum visited companies of 4096 exceeded.",
            ));
        }
        for vals in &vals_list {
            erp_sdk::__private::base_check_write_fields(tx, records, vals)?;
        }
        let guard = crate::company_lifecycle::Guard::enter(tx, &[])?;
        crate::company_lifecycle::fence(tx).await?;
        let defaults: ValueMap = records
            .call(tx, "default_get", (vec!["parent_id".to_owned()],))
            .await?;
        let defaults = records.vals_from_values(&defaults)?;
        for vals in &mut vals_list {
            if !vals.contains("parent_id")
                && let Some(parent) = defaults.get("parent_id")
            {
                vals.insert("parent_id".into(), parent.clone());
            }
            match vals.get("parent_id") {
                Some(Cell::Id(id)) => {
                    let parent = records.browse([*id])?;
                    let currency = crate::company_lifecycle::root_currency(tx, &parent).await?;
                    if !vals.contains("currency_id") {
                        vals.insert("currency_id".into(), currency);
                    }
                }
                Some(Cell::Null | Cell::Bool(false)) | None => {}
                Some(_) => {
                    return Err(Error::validation(
                        "Company parent must be a company reference or false.",
                    ));
                }
            }
        }
        let mut slots = Vec::new();
        let mut partner_vals = Vec::new();
        for (slot, vals) in vals_list.iter().enumerate() {
            let has_partner = vals
                .get("partner_id")
                .is_some_and(|cell| !matches!(cell, Cell::Null | Cell::Bool(false)));
            let name = match vals.get("name") {
                Some(Cell::Str(name)) if !name.trim().is_empty() => Some(name.clone()),
                _ => None,
            };
            let Some(name) = name.filter(|_| !has_partner) else {
                continue;
            };
            let mut partner = RawVals::new().set_cell("name", Cell::Str(name));
            for field in PARTNER_MIRRORED {
                if let Some(cell) = vals.get(field)
                    && !matches!(cell, Cell::Null | Cell::Bool(false))
                {
                    partner = partner.set_cell(field, cell.clone());
                }
            }
            slots.push(slot);
            partner_vals.push(partner);
        }
        if !partner_vals.is_empty() {
            let mut context = this.env().context().clone();
            context.remove("default_parent_id");
            let env = this.env().with_context_replaced(context);
            let partners = env.records("res.partner")?.create(tx, partner_vals).await?;
            if partners.ids().len() != slots.len() {
                return Err(Error::programming(format!(
                    "res.company create: {} partners created for {} companies",
                    partners.ids().len(),
                    slots.len()
                )));
            }
            for (slot, partner) in slots.into_iter().zip(partners.ids()) {
                vals_list[slot].insert("partner_id".into(), Cell::Id(partner));
            }
        }
        let created: Records = sup.call(tx, this.as_records(), (vals_list,)).await?;
        guard.admit(&created.ids())?;
        Ok(created)
    }

    /// Parentage is fixed at creation; lifecycle changes serialize across all nodes.
    #[method(override)]
    async fn write(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        erp_sdk::__private::base_check_write_fields(tx, records, &vals)?;
        if vals.contains("parent_id") {
            return Err(Error::user("The company hierarchy cannot be changed."));
        }
        let lifecycle = vals.contains("currency_id") || vals.contains("active");
        let archives = matches!(vals.get("active"), Some(Cell::Bool(false)));
        let propagates = archives || vals.contains("currency_id");
        // Only lifecycle writes (active, currency) are bounded and serialized. Other fields, such
        // as a name written on many companies at once, keep the ordinary write path (
        // bounds lifecycle operations, not every write).
        let guard = if lifecycle {
            Some(crate::company_lifecycle::Guard::enter(tx, &records.ids())?)
        } else {
            None
        };
        let descendants = match &guard {
            Some(guard) => {
                crate::company_lifecycle::fence(tx).await?;
                // Unarchiving does not cascade, so it needs no subtree discovery.
                if propagates {
                    let descendants =
                        crate::company_lifecycle::descendants(tx, records, guard).await?;
                    crate::company_lifecycle::admit_write(tx, &descendants).await?;
                    if archives {
                        crate::company_lifecycle::protect_archive(
                            tx,
                            &records.union(&descendants)?,
                        )
                        .await?;
                    }
                    descendants
                } else {
                    records.browse([])?
                }
            }
            None => records.browse([])?,
        };
        sup.call(tx, records, (vals.clone(),)).await?;
        if archives && !descendants.is_empty() {
            // Direct children recurse through normal overrides; preflight above included the entire tree.
            let direct = records.related(tx, "all_child_ids").await?;
            if !direct.is_empty() {
                direct
                    .write(tx, RawVals::new().set_cell("active", Cell::Bool(false)))
                    .await?;
            }
        }
        if let Some(currency) = vals.get("currency_id") {
            let guard = guard.as_ref().ok_or_else(|| {
                Error::programming("Company currency write ran without its lifecycle guard")
            })?;
            for company in records.iter() {
                if company.related(tx, "parent_id").await?.is_empty() {
                    let branches =
                        crate::company_lifecycle::descendants(tx, &company, guard).await?;
                    if !branches.is_empty() {
                        branches
                            .write(tx, RawVals::new().set_cell("currency_id", currency.clone()))
                            .await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Explicit NULL is not an archive command; refuse it rather than bypassing the cascade.
    /// The post-write user check also protects composition with lower addon overrides.
    #[constrains("active")]
    async fn _check_company_active(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for company in this.as_records().iter() {
            match company.cell(tx, "active").await? {
                Cell::Bool(false) => {
                    crate::company_lifecycle::protect_archive(tx, &company).await?
                }
                Cell::Bool(true) => {}
                _ => return Err(Error::validation("Company active must be a boolean.")),
            }
        }
        Ok(())
    }

    /// A branch can never persist a currency different from its root.
    #[constrains("currency_id", "parent_id")]
    async fn _check_branch_currency(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for company in this.as_records().iter() {
            let parent = company.related(tx, "parent_id").await?;
            if !parent.is_empty()
                && company.cell(tx, "currency_id").await?
                    != crate::company_lifecycle::root_currency(tx, &company).await?
            {
                return Err(Error::validation(
                    "A branch must use its root company's currency.",
                ));
            }
        }
        Ok(())
    }

    /// Deletion shares lifecycle lock ordering with creation and user transitions.
    #[method(override)]
    async fn unlink(this: &RecordSet<ResCompany>, tx: &mut Transaction, sup: Super) -> Result<()> {
        this.as_records()
            .check_access(tx, Operation::Unlink)
            .await?;
        crate::company_lifecycle::fence(tx).await?;
        sup.call(tx, this.as_records(), ()).await
    }

    /// Fill the draft currency when a parent is selected; persistence is checked separately.
    #[onchange("parent_id")]
    async fn _onchange_branch_parent(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let parent = this.as_records().related(tx, "parent_id").await?;
        if !parent.is_empty() {
            let currency = crate::company_lifecycle::root_currency(tx, &parent).await?;
            this.as_records()
                .write_base(tx, RawVals::new().set_cell("currency_id", currency))
                .await?;
        }
        Ok(OnchangeResult::none())
    }

    /// Open direct branches, including archived records, with ordinary caller access.
    #[method]
    #[rpc(effect = read)]
    async fn action_all_company_branches(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records();
        records.check_access(tx, Operation::Read).await?;
        let id = records.ensure_one()?.int()?;
        if records.exists(tx).await?.is_empty() {
            return Err(Error::missing(
                "Branches action requires an existing company.",
            ));
        }
        let context = ValueMap::from([
            ("default_parent_id".to_owned(), Value::Int(id)),
            ("active_test".to_owned(), Value::Bool(false)),
        ]);
        Ok(Value::Map(Arc::new(ValueMap::from([
            ("type".to_owned(), Value::str("ir.actions.act_window")),
            ("name".to_owned(), Value::str("Branches")),
            ("res_model".to_owned(), Value::str("res.company")),
            ("target".to_owned(), Value::str("current")),
            (
                "views".to_owned(),
                Value::list([
                    Value::list([Value::False, Value::str("list")]),
                    Value::list([Value::False, Value::str("form")]),
                ]),
            ),
            (
                "domain".to_owned(),
                Value::list([Value::list([
                    Value::str("parent_id"),
                    Value::str("="),
                    Value::Int(id),
                ])]),
            ),
            ("context".to_owned(), Value::Map(Arc::new(context))),
        ]))))
    }

    /// A company assignment activates its currency and writes its row version.
    ///
    /// The write also serializes assignment against an archive from an older
    /// repeatable-read snapshot. Never skip it merely because active is already true.
    #[constrains("currency_id")]
    async fn _activate_company_currency(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let mut ids = this
            .mapped(tx, ResCompany::CURRENCY_ID)
            .await?
            .into_iter()
            .flatten()
            .map(RecId::get)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(());
        }
        let currencies = this.env().records("res.currency")?.browse(ids)?;
        currencies
            .write(tx, RawVals::new().set_cell("active", Cell::Bool(true)))
            .await
    }

    /// A draft branch uses its context parent's root currency. Other companies resolve the
    /// acting user's home company, independent of the selected company.
    /// Lookup errors propagate; an absent user/company/currency supplies no default.
    #[method(override)]
    async fn default_get(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let wants_currency = names.iter().any(|n| n == "currency_id");
        let wants_parent = names.iter().any(|n| n == "parent_id");
        let mut requested = names;
        if wants_currency && !wants_parent {
            requested.push("parent_id".to_owned());
        }
        let mut out = sup.call(tx, this.as_records(), (requested,)).await?;
        if wants_currency {
            let defaults = this.as_records().vals_from_values(&out)?;
            if let Some(Cell::Id(parent)) = defaults.get("parent_id") {
                let parent = this.as_records().browse([*parent])?;
                let currency = crate::company_lifecycle::root_currency(tx, &parent).await?;
                let currency = <Option<RecId> as erp_sdk::FieldValue>::from_cell(&currency)?;
                if let Some(currency) = currency {
                    out.insert("currency_id".to_owned(), Value::Int(currency.get()));
                }
            }
        }
        if !wants_parent {
            out.shift_remove("parent_id");
        }
        if wants_currency && !out.contains_key("currency_id") {
            let env = this.env();
            let users = env
                .records("res.users")?
                .browse([env.uid()])?
                .exists(tx)
                .await?;
            if !users.is_empty() {
                let cell = users.cell(tx, "company_id").await?;
                let company_id = <Option<RecId> as erp_sdk::FieldValue>::from_cell(&cell)?;
                if let Some(company_id) = company_id {
                    let company = this.browse([company_id.get()])?;
                    if let Some(currency_id) = company.get(tx, ResCompany::CURRENCY_ID).await? {
                        out.insert("currency_id".to_owned(), Value::Int(currency_id.get()));
                    }
                }
            }
        }
        Ok(out)
    }
}
