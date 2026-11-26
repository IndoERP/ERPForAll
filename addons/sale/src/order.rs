// SPDX-License-Identifier: MIT OR Apache-2.0
//! The state operations are
//! in [`crate::order_ops`].

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;

use erp_addon_base::ResPartner;
use erp_addon_mail::{MailActivityMixin, MailThread};
use erp_sdk::prelude::*;
use erp_sdk::{Inherits, Model, Operation, ValueMap};

/// The code of the sequence record orders draw their number from (`sale/data/ir_sequence_data.xml:4-10`).
pub const SEQUENCE_CODE: &str = "sale.order";
/// The name of an order before its number is drawn.
pub const PLACEHOLDER: &str = "New";
/// The states and their labels.
pub const STATES: [(&str, &str); 4] = [
    ("draft", "Quotation"),
    ("sent", "Quotation Sent"),
    ("sale", "Sales Order"),
    ("cancel", "Cancelled"),
];
/// Fields only the operations and the status computes change: never written by a caller, also
/// not with sudo, on create, on write or as a `default_<field>` context key; `name` comes last, a
/// create may give it (V6, V11, V12).
const OPERATION_FIELDS: [&str; 6] = [
    "state",
    "locked",
    "invoicing_closed",
    "invoice_status",
    "delivery_status",
    "name",
];
/// The fields of `OPERATION_FIELDS` a create refuses.
const CREATE_REFUSED: &[&str] = OPERATION_FIELDS.split_at(5).0;

fn now(_ctx: &erp_sdk::DefaultContext) -> Value {
    Value::DateTime(DateTime::now())
}

/// `sale.order`.
#[model(
    name = "sale.order",
    description = "Sales Order",
    inherit = [MailThread, MailActivityMixin],
    order = "date_order DESC, id DESC",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
)]
pub struct SaleOrder {
    /// The order number, drawn at creation; a name given at create is kept.
    #[field(
        char,
        required = true,
        copy = false,
        index = "trigram",
        string = "Order Reference"
    )]
    pub name: Option<Arc<str>>,
    /// The company of the order; default: the current company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// The customer.
    #[field(
        many2one,
        comodel = "res.partner",
        required = true,
        index = true,
        tracking = true,
        string = "Customer"
    )]
    pub partner_id: Option<RecId>,
    /// The lifecycle state, changed by the operations only.
    #[field(
        selection,
        selection = [
            ("draft", "Quotation"),
            ("sent", "Quotation Sent"),
            ("sale", "Sales Order"),
            ("cancel", "Cancelled"),
        ],
        default = "draft",
        index = true,
        copy = false,
        tracking = true,
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Whether a confirmed order is locked, changed by lock and unlock only.
    #[field(
        boolean,
        default = false,
        copy = false,
        tracking = true,
        string = "Locked"
    )]
    pub locked: bool,
    /// Creation time of a quotation, confirmation time of an order.
    #[field(
        datetime,
        required = true,
        copy = false,
        index = true,
        default_fn = now,
        string = "Order Date"
    )]
    pub date_order: Option<DateTime>,
    /// The customer's own reference.
    #[field(char, copy = false, string = "Customer Reference")]
    pub client_order_ref: Option<Arc<str>>,
    /// The source document.
    #[field(char, string = "Source Document")]
    pub origin: Option<Arc<str>>,
    /// The promised delivery date.
    #[field(datetime, copy = false, string = "Delivery Date")]
    pub commitment_date: Option<DateTime>,
    /// The quotation's expiry date.
    #[field(date, copy = false, string = "Expiration")]
    pub validity_date: Option<Date>,
    /// The invoicing address: the customer's invoice address unless chosen.
    #[field(
        many2one,
        comodel = "res.partner",
        required = true,
        compute = "_compute_partner_invoice_id",
        store = true,
        readonly = false,
        index = true,
        string = "Invoice Address"
    )]
    pub partner_invoice_id: Option<RecId>,
    /// The delivery address: the customer's delivery address unless chosen.
    #[field(
        many2one,
        comodel = "res.partner",
        required = true,
        compute = "_compute_partner_shipping_id",
        store = true,
        readonly = false,
        index = true,
        string = "Delivery Address"
    )]
    pub partner_shipping_id: Option<RecId>,
    /// The salesperson.
    // shortcut: not tracked (the reference tracks it): the registry refuses tracking on a
    // computed field until recomputed fields are tracked; add `tracking = true` then.
    #[field(
        many2one,
        comodel = "res.users",
        compute = "_compute_user_id",
        store = true,
        readonly = false,
        index = true,
        string = "Salesperson"
    )]
    pub user_id: Option<RecId>,
    /// The sales team.
    // shortcut: not tracked, for the same reason as `user_id`.
    #[field(
        many2one,
        comodel = "crm.team",
        compute = "_compute_team_id",
        store = true,
        readonly = false,
        index = true,
        ondelete = "set null",
        string = "Sales Team"
    )]
    pub team_id: Option<RecId>,
    /// Tags.
    #[field(
        many2many,
        comodel = "crm.tag",
        relation = "sale_order_tag_rel",
        column1 = "order_id",
        column2 = "tag_id",
        string = "Tags"
    )]
    pub tag_ids: Vec<RecId>,
    /// Terms and conditions.
    #[field(html, string = "Terms and conditions")]
    pub note: Option<Arc<str>>,
    /// The lines; copied with the order.
    #[field(
        one2many,
        comodel = "sale.order.line",
        inverse_name = "order_id",
        copy = true,
        string = "Order Lines"
    )]
    pub order_line: Vec<RecId>,
    /// Invoicing closed by hand: the order counts as fully invoiced.
    #[field(
        boolean,
        default = false,
        copy = false,
        tracking = true,
        string = "Invoicing Closed"
    )]
    pub invoicing_closed: bool,
    /// The invoice status of the order, from its lines ([`crate::order_status`]).
    #[field(
        selection,
        selection = [
            ("upselling", "Upselling Opportunity"),
            ("invoiced", "Fully Invoiced"),
            ("to invoice", "To Invoice"),
            ("no", "Nothing to Invoice"),
        ],
        compute = "_compute_invoice_status",
        store = true,
        string = "Invoice Status"
    )]
    pub invoice_status: Option<Arc<str>>,
    /// How far the goods of a confirmed order are delivered ([`crate::order_status`]).
    #[field(
        selection,
        selection = [("pending", "Not Delivered"), ("partial", "Partially Delivered"), ("full", "Fully Delivered")],
        compute = "_compute_delivery_status",
        store = true,
        string = "Delivery Status"
    )]
    pub delivery_status: Option<Arc<str>>,
    /// How many customer invoices and credit notes of the order the caller may read
    /// ([`crate::invoice_buttons`]).
    #[field(integer, compute = "_compute_invoice_count", string = "Invoice Count")]
    pub invoice_count: i32,
    /// True while Cancel would be refused: an invoice of the order is posted, or a bridge's
    /// downstream document holds it ([`crate::order_ops`], `_cancel_blocked`). The form hides
    /// Cancel with it.
    #[field(
        boolean,
        compute = "_compute_cancel_blocked",
        string = "Cancel Blocked"
    )]
    pub cancel_blocked: bool,
    /// The pricelist new lines are priced from; none: the products' sales prices. Not changed
    /// once the order is confirmed.
    #[field(
        many2one,
        comodel = "product.pricelist",
        check_company = true,
        tracking = true,
        string = "Pricelist"
    )]
    pub pricelist_id: Option<RecId>,
    /// The currency: the pricelist's, else the company's.
    #[field(
        many2one,
        comodel = "res.currency",
        compute = "_compute_currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Units of the order currency for one unit of the company currency on the order day.
    #[field(
        float,
        digits = (12, 6),
        compute = "_compute_currency_rate",
        store = true,
        string = "Currency Rate"
    )]
    pub currency_rate: Decimal,
    /// The fiscal position that maps the taxes of new lines: the customer's, found from the
    /// customer and the delivery address in the order's company, unless chosen.
    #[field(
        many2one,
        comodel = "account.fiscal.position",
        compute = "_compute_fiscal_position_id",
        store = true,
        readonly = false,
        check_company = true,
        string = "Fiscal Position"
    )]
    pub fiscal_position_id: Option<RecId>,
    /// The sum of the line subtotals.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Untaxed Amount"
    )]
    pub amount_untaxed: Decimal,
    /// The sum of the line taxes.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Taxes"
    )]
    pub amount_tax: Decimal,
    /// Untaxed amount plus taxes.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Total"
    )]
    pub amount_total: Decimal,
    /// Optional quotation template used to populate lines and optional products.
    #[field(
        many2one,
        comodel = "sale.order.template",
        check_company = true,
        string = "Quotation Template"
    )]
    pub sale_order_template_id: Option<RecId>,
    /// Optional upsell products suggested on this quotation.
    #[field(
        one2many,
        comodel = "sale.order.option",
        inverse_name = "order_id",
        copy = true,
        string = "Optional Products"
    )]
    pub sale_order_option_ids: Vec<RecId>,
    /// Whether customer online signature is requested before confirmation.
    #[field(boolean, default = true, string = "Online Signature")]
    pub require_signature: bool,
    /// Whether customer online payment is requested before confirmation.
    #[field(boolean, default = false, string = "Online Payment")]
    pub require_payment: bool,
    /// Full name of the customer representative who digitally signed the order.
    #[field(char, copy = false, tracking = true, string = "Signed By")]
    pub signed_by: Option<Arc<str>>,
    /// Timestamp when the order was digitally signed.
    #[field(datetime, copy = false, string = "Signed On")]
    pub signed_on: Option<DateTime>,
    /// Total exact-decimal profit margin across all product lines.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_margin",
        store = true,
        groups = "sales_team.group_sale_salesman",
        string = "Margin"
    )]
    pub margin: Decimal,
    /// Overall profit margin percentage.
    #[field(
        float,
        digits = (16, 2),
        compute = "_compute_margin",
        store = true,
        groups = "sales_team.group_sale_salesman",
        string = "Margin (%)"
    )]
    pub margin_percent: Decimal,
}

/// The header fields a locked order refuses besides its protected fields: the salesperson and the
/// sales team.
const LOCKED_ONLY: [&str; 2] = ["user_id", "team_id"];

/// The fields of `M` a locked order protects (R7): built from field handles of `M`, so a
/// field that does not exist cannot be named. A satellite adds its own fields in its override of
/// `_get_protected_fields`.
pub struct ProtectedFields<M> {
    names: BTreeSet<&'static str>,
    _model: PhantomData<fn() -> M>,
}

impl<M> ProtectedFields<M> {
    /// No field.
    #[must_use]
    pub fn new() -> Self {
        Self {
            names: BTreeSet::new(),
            _model: PhantomData,
        }
    }

    /// The set with `field` added: a field of `M` or of an extension of `M`.
    #[must_use]
    pub fn with<P: Model, T>(mut self, field: Field<P, T>) -> Self
    where
        M: Inherits<P>,
    {
        self.names.insert(field.name());
        self
    }

    /// Whether the field named `name` is protected.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// The protected field names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.names.iter().copied()
    }
}

impl<M> Default for ProtectedFields<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> Clone for ProtectedFields<M> {
    fn clone(&self) -> Self {
        Self {
            names: self.names.clone(),
            _model: PhantomData,
        }
    }
}

impl<M> std::fmt::Debug for ProtectedFields<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(&self.names).finish()
    }
}

/// What the rules read of one order.
#[derive(Debug, Clone)]
pub(crate) struct Header {
    pub(crate) id: RecId,
    pub(crate) name: String,
    pub(crate) state: String,
    pub(crate) locked: bool,
}

impl Header {
    /// "S00001 (Sales Order)".
    pub(crate) fn describe(&self) -> String {
        let label = STATES
            .iter()
            .find(|(key, _)| *key == self.state)
            .map_or(self.state.as_str(), |(_, label)| label);
        format!("{} ({label})", self.name)
    }
}

/// The name, state and lock of each order of `records`, in the order of `records`.
///
/// # Errors
/// The errors of the reads.
pub(crate) async fn headers(tx: &mut Transaction, records: &Records) -> Result<Vec<Header>> {
    let set = RecordSet::<SaleOrder>::try_from(records.clone())?;
    let names = set.mapped(tx, SaleOrder::NAME).await?;
    let states = set.mapped(tx, SaleOrder::STATE).await?;
    let locked = set.mapped(tx, SaleOrder::LOCKED).await?;
    let mut heads = Vec::with_capacity(names.len());
    for (id, ((name, state), locked)) in records
        .rec_ids()
        .iter()
        .zip(names.into_iter().zip(states).zip(locked))
    {
        // a stored order always has a state; reading none as a draft would let a damaged row be edited
        let state =
            state.ok_or_else(|| Error::invariant(format!("sales order {id:?} has no state")))?;
        heads.push(Header {
            id: *id,
            name: name.as_deref().unwrap_or(PLACEHOLDER).to_owned(),
            state: state.to_string(),
            locked,
        });
    }
    Ok(heads)
}

/// Whether the order `order` has a line of any kind.
async fn has_lines(tx: &mut Transaction, order: RecId) -> Result<bool> {
    let rows = tx
        .query(
            "SELECT 1 FROM sale_order_line WHERE order_id = $1 LIMIT 1",
            &[&order.get()],
        )
        .await?;
    Ok(!rows.is_empty())
}

/// The label of a field of `model`.
pub(crate) fn label_of(tx: &Transaction, model: &str, field: &str) -> Result<String> {
    tx.registry()
        .model(model)
        .and_then(|spec| spec.field(field))
        .map(|spec| spec.string.clone())
        .ok_or_else(|| Error::internal(format!("{model} has no field '{field}'")))
}

/// The label of a field of `sale.order`.
pub(crate) fn label(tx: &Transaction, field: &str) -> Result<String> {
    label_of(tx, "sale.order", field)
}

/// Refuse a client write of a field only the operations change.
fn refuse_operation_fields(tx: &Transaction, vals: &RawVals, fields: &[&str]) -> Result<()> {
    match fields.iter().find(|field| vals.contains(field)) {
        Some(field) => Err(Error::validation(format!(
            "The field \"{}\" of a sales order cannot be written directly.",
            label(tx, field)?
        ))),
        None => Ok(()),
    }
}

/// Refuse a `default_<field>` context key for a field the create guards of `what` refuse: the base
/// create applies context defaults after the override has read the values, so such a key would go
/// around the guard.
pub(crate) fn refuse_context_defaults(env: &Env, what: &str, fields: &[&str]) -> Result<()> {
    match fields
        .iter()
        .find(|field| env.context().contains(&format!("default_{field}")))
    {
        Some(field) => Err(Error::validation(format!(
            "The context key default_{field} is not accepted when a {what} is created."
        ))),
        None => Ok(()),
    }
}

/// Whether `field` is given to a create: in the values or as a `default_<field>` context key.
pub(crate) fn given(env: &Env, vals: &RawVals, field: &str) -> bool {
    vals.contains(field) || env.context().contains(&format!("default_{field}"))
}

/// The record id given to a create for the many2one `field`, from the values or else from the
/// `default_<field>` context key.
///
/// # Errors
/// `Validation` for a value that is not a record id.
pub(crate) fn given_id(env: &Env, vals: &RawVals, field: &str) -> Result<Option<RecId>> {
    let id = match vals.get(field) {
        Some(Cell::Id(id)) => Some(*id),
        Some(Cell::Int(id)) => Some(i64::from(*id)),
        Some(Cell::Null | Cell::Bool(false)) => None,
        Some(other) => {
            return Err(Error::validation(format!(
                "{field} takes a record id, not {other:?}."
            )));
        }
        None => match env.context().get(&format!("default_{field}")) {
            None | Some(Value::False | Value::Bool(false)) => None,
            Some(Value::Int(id)) => Some(id),
            Some(other) => {
                return Err(Error::validation(format!(
                    "default_{field} takes a record id, not {other:?}."
                )));
            }
        },
    };
    Ok(id.and_then(RecId::from_real))
}

/// The addresses of `kinds` of `partner` from `address_get`; the partner itself for a kind it has
/// no contact of.
async fn partner_addresses(
    tx: &mut Transaction,
    env: &Env,
    partner: RecId,
    kinds: &[&str],
) -> Result<Vec<Option<RecId>>> {
    let map: ValueMap = env
        .records("res.partner")?
        .browse_ids([partner])
        .call(
            tx,
            "address_get",
            (kinds
                .iter()
                .map(|kind| (*kind).to_owned())
                .collect::<Vec<_>>(),),
        )
        .await?;
    Ok(kinds
        .iter()
        .map(|kind| match map.get(*kind) {
            Some(Value::Int(id)) => RecId::from_real(*id),
            _ => Some(partner),
        })
        .collect())
}

/// The invoice and delivery addresses a create does not give, from its customer: both fields are
/// required and their compute runs only after the INSERT, so the values are set before it.
async fn fill_addresses(tx: &mut Transaction, env: &Env, vals: &mut RawVals) -> Result<()> {
    let missing: Vec<(&'static str, &'static str)> = [
        ("partner_invoice_id", "invoice"),
        ("partner_shipping_id", "delivery"),
    ]
    .into_iter()
    .filter(|(field, _)| !given(env, vals, field))
    .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let Some(partner) = given_id(env, vals, "partner_id")? else {
        return Ok(());
    };
    let kinds: Vec<&str> = missing.iter().map(|(_, kind)| *kind).collect();
    let addresses = partner_addresses(tx, env, partner, &kinds).await?;
    for ((field, _), address) in missing.into_iter().zip(addresses) {
        if let Some(address) = address {
            vals.insert(field.into(), Cell::Id(address.get()));
        }
    }
    Ok(())
}

/// The current values of a stored field of persisted orders, as the database holds them before
/// a pending recompute (the reference's `_origin` value).
async fn stored_ids(
    tx: &mut Transaction,
    records: &Records,
    column: &'static str,
) -> Result<BTreeMap<i64, i64>> {
    let ids: Vec<i64> = records
        .rec_ids()
        .iter()
        .filter_map(|id| id.as_real().map(|id| id.get()))
        .collect();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    // the column name is one of two constants of this module, never caller text
    let sql = format!(
        "SELECT id::int8, {column}::int8 FROM sale_order WHERE id = ANY($1) AND {column} IS NOT NULL"
    );
    Ok(tx
        .query(&sql, &[&ids])
        .await?
        .iter()
        .map(|row| (row.get::<_, i64>(0), row.get::<_, i64>(1)))
        .collect())
}

/// The address of `kind` of each order's customer, `None` without a customer.
async fn addresses(
    this: &RecordSet<SaleOrder>,
    tx: &mut Transaction,
    kind: &str,
) -> Result<Vec<Option<RecId>>> {
    let partners = this.mapped(tx, SaleOrder::PARTNER_ID).await?;
    let env = this.env().clone();
    let mut found: BTreeMap<RecId, Option<RecId>> = BTreeMap::new();
    for partner in partners.iter().flatten() {
        if found.contains_key(partner) {
            continue;
        }
        let address = partner_addresses(tx, &env, *partner, &[kind])
            .await?
            .pop()
            .flatten();
        found.insert(*partner, address);
    }
    Ok(partners
        .into_iter()
        .map(|partner| partner.and_then(|partner| found.get(&partner).copied().flatten()))
        .collect())
}

/// (user, company, present team) of the default-team lookup.
type TeamKey = (Option<i64>, Option<i64>, Option<i64>);

#[methods(SaleOrder, addon = "sale")]
impl SaleOrder {
    #[compute(depends = ["partner_id"])]
    async fn _compute_partner_invoice_id(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrder>,
    ) -> Result<()> {
        for (index, address) in addresses(this, tx, "invoice")
            .await?
            .into_iter()
            .enumerate()
        {
            out.set(index, SaleOrder::PARTNER_INVOICE_ID, address)?;
        }
        Ok(())
    }

    #[compute(depends = ["partner_id"])]
    async fn _compute_partner_shipping_id(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrder>,
    ) -> Result<()> {
        for (index, address) in addresses(this, tx, "delivery")
            .await?
            .into_iter()
            .enumerate()
        {
            out.set(index, SaleOrder::PARTNER_SHIPPING_ID, address)?;
        }
        Ok(())
    }

    /// A saved order keeps its salesperson. Otherwise: the customer's salesperson, else that of
    /// the customer's commercial partner, else the current user when they are a sales user.
    #[compute(depends = ["partner_id"])]
    async fn _compute_user_id(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrder>,
    ) -> Result<()> {
        let kept = stored_ids(tx, this.as_records(), "user_id").await?;
        let partners = this.mapped(tx, SaleOrder::PARTNER_ID).await?;
        let env = this.env().clone();
        // the system principal has no groups to evaluate and is nobody's salesperson; superuser
        // mode (a stored compute runs in it) keeps the user, so it is not the test
        let salesman = env.uid() != erp_sdk::SUPERUSER_ID.get()
            && tx.has_group(&env, "sales_team.group_sale_salesman").await?;
        let current = RecId::from_real(env.uid()).filter(|_| salesman);
        let customers = RecordSet::<ResPartner>::try_from(
            env.records("res.partner")?
                .browse_ids(partners.iter().flatten().copied()),
        )?;
        let own = customers.mapped(tx, ResPartner::USER_ID).await?;
        let commercial = customers
            .mapped(tx, ResPartner::COMMERCIAL_PARTNER_ID)
            .await?;
        let parents = RecordSet::<ResPartner>::try_from(
            env.records("res.partner")?
                .browse_ids(commercial.iter().flatten().copied()),
        )?;
        let parent_users = parents.mapped(tx, ResPartner::USER_ID).await?;
        let parent_user: BTreeMap<RecId, RecId> = parents
            .as_records()
            .rec_ids()
            .iter()
            .zip(parent_users)
            .filter_map(|(parent, user)| Some((*parent, user?)))
            .collect();
        let salesperson: BTreeMap<RecId, Option<RecId>> = customers
            .as_records()
            .rec_ids()
            .iter()
            .zip(own.into_iter().zip(commercial))
            .map(|(customer, (own, commercial))| {
                let inherited = commercial.and_then(|c| parent_user.get(&c).copied());
                (*customer, own.or(inherited))
            })
            .collect();
        let ids = this.as_records().rec_ids().to_vec();
        for (index, (id, partner)) in ids.iter().zip(partners).enumerate() {
            let value = match id.as_real().and_then(|id| kept.get(&id.get())) {
                Some(user) => RecId::from_real(*user),
                None => partner
                    .and_then(|partner| salesperson.get(&partner).copied().flatten().or(current)),
            };
            out.set(index, SaleOrder::USER_ID, value)?;
        }
        Ok(())
    }

    /// The default team of the salesperson in the order's company, with the order's present team
    /// as the preferred one (`crm.team._get_default_team_id`).
    #[compute(depends = ["user_id"])]
    async fn _compute_team_id(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrder>,
    ) -> Result<()> {
        let present = stored_ids(tx, this.as_records(), "team_id").await?;
        let users = this.mapped(tx, SaleOrder::USER_ID).await?;
        let companies = this.mapped(tx, SaleOrder::COMPANY_ID).await?;
        let teams = this.env().records("crm.team")?;
        let mut cache: BTreeMap<TeamKey, Option<i64>> = BTreeMap::new();
        let ids = this.as_records().rec_ids().to_vec();
        for (index, (id, (user, company))) in
            ids.iter().zip(users.into_iter().zip(companies)).enumerate()
        {
            let default = id.as_real().and_then(|id| present.get(&id.get()).copied());
            let key = (user.map(RecId::get), company.map(RecId::get), default);
            let team = match cache.get(&key) {
                Some(team) => *team,
                None => {
                    let team: Option<i64> = teams.call(tx, "_get_default_team_id", key).await?;
                    cache.insert(key, team);
                    team
                }
            };
            out.set(index, SaleOrder::TEAM_ID, team.and_then(RecId::from_real))?;
        }
        Ok(())
    }

    /// The pricelist's currency, else the company's.
    #[compute(depends = ["pricelist_id", "company_id"])]
    async fn _compute_currency_id(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrder>,
    ) -> Result<()> {
        let pricelists = this.mapped(tx, SaleOrder::PRICELIST_ID).await?;
        let companies = this.mapped(tx, SaleOrder::COMPANY_ID).await?;
        let env = this.env().clone();
        let mut found: BTreeMap<(&str, RecId), Option<RecId>> = BTreeMap::new();
        for (index, (pricelist, company)) in pricelists.iter().zip(&companies).enumerate() {
            let (model, owner) = match (pricelist, company) {
                (Some(pricelist), _) => ("product.pricelist", *pricelist),
                (None, Some(company)) => ("res.company", *company),
                (None, None) => {
                    out.set(index, SaleOrder::CURRENCY_ID, None)?;
                    continue;
                }
            };
            let currency = match found.get(&(model, owner)) {
                Some(currency) => *currency,
                None => {
                    let cell = env
                        .records(model)?
                        .browse_ids([owner])
                        .cell(tx, "currency_id")
                        .await?;
                    let currency = match cell {
                        Cell::Id(id) => RecId::from_real(id),
                        Cell::Null | Cell::Bool(false) => None,
                        other => {
                            return Err(Error::invariant(format!(
                                "{model} {owner:?} currency is {other:?}"
                            )));
                        }
                    };
                    found.insert((model, owner), currency);
                    currency
                }
            };
            out.set(index, SaleOrder::CURRENCY_ID, currency)?;
        }
        Ok(())
    }

    /// The rate from the company currency to the order currency on the order day, with the rate
    /// rule of every conversion.
    #[compute(depends = ["currency_id", "date_order", "company_id"])]
    async fn _compute_currency_rate(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrder>,
    ) -> Result<()> {
        use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RateBook};
        use erp_sdk::money::{Increment, RateDateRule};
        let currencies = this.mapped(tx, SaleOrder::CURRENCY_ID).await?;
        let companies = this.mapped(tx, SaleOrder::COMPANY_ID).await?;
        let dates = this.mapped(tx, SaleOrder::DATE_ORDER).await?;
        let env = this.env().clone();
        let places = Increment::from_places(6)?;
        for (index, ((currency, company), date)) in
            currencies.iter().zip(&companies).zip(&dates).enumerate()
        {
            let (Some(currency), Some(company), Some(date)) = (currency, company, date) else {
                out.set(index, SaleOrder::CURRENCY_RATE, Decimal::ZERO)?;
                continue;
            };
            let company_currency = match env
                .records("res.company")?
                .browse_ids([*company])
                .cell(tx, "currency_id")
                .await?
            {
                Cell::Id(id) => RecId::from_real(id)
                    .ok_or_else(|| Error::invariant(format!("currency id {id}")))?,
                other => {
                    return Err(Error::invariant(format!(
                        "company {company:?} currency is {other:?}"
                    )));
                }
            };
            let day = crate::amounts::order_day(&env, *date)?;
            let book = RateBook::load(
                &env,
                tx,
                *company,
                day,
                RateDateRule::StrictlyBefore,
                &[company_currency, *currency],
            )
            .await?;
            let rate = book
                .snapshot(company_currency, *currency)?
                .factor()?
                .quantize(&places, CURRENCY_ROUNDING_MODE)?;
            out.set(index, SaleOrder::CURRENCY_RATE, rate)?;
        }
        Ok(())
    }

    /// The fiscal position of the customer and the delivery address in the order's company.
    #[compute(depends = ["partner_id", "partner_shipping_id", "company_id"])]
    async fn _compute_fiscal_position_id(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrder>,
    ) -> Result<()> {
        let partners = this.mapped(tx, SaleOrder::PARTNER_ID).await?;
        let shipping = this.mapped(tx, SaleOrder::PARTNER_SHIPPING_ID).await?;
        let companies = this.mapped(tx, SaleOrder::COMPANY_ID).await?;
        let env = this.env().clone();
        let mut found: BTreeMap<(i64, Option<i64>, i64), Option<i64>> = BTreeMap::new();
        for (index, ((partner, delivery), company)) in
            partners.iter().zip(&shipping).zip(&companies).enumerate()
        {
            let (Some(partner), Some(company)) = (partner, company) else {
                out.set(index, SaleOrder::FISCAL_POSITION_ID, None)?;
                continue;
            };
            let key = (partner.get(), delivery.map(RecId::get), company.get());
            let position = match found.get(&key) {
                Some(position) => *position,
                None => {
                    let position: Option<i64> = env
                        .with_company(key.2)?
                        .records("account.fiscal.position")?
                        .call(tx, "_get_fiscal_position", (key.0, key.1))
                        .await?;
                    found.insert(key, position);
                    position
                }
            };
            out.set(
                index,
                SaleOrder::FISCAL_POSITION_ID,
                position.and_then(RecId::from_real),
            )?;
        }
        Ok(())
    }

    /// Recomputes live line amounts and order totals (`amount_untaxed`, `amount_tax`,
    /// `amount_total`) in memory before Save.
    #[onchange(
        "order_line",
        "pricelist_id",
        "currency_id",
        "fiscal_position_id",
        "date_order"
    )]
    async fn _onchange_order_totals(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        crate::amounts::onchange_order_totals(tx, this.as_records()).await
    }

    /// The header fields a locked order refuses to change: customer, invoice address, delivery
    /// address, promised delivery date and the lines (V13). A satellite adds its
    /// own through an override that calls `sup`.
    #[method]
    async fn _get_protected_fields(
        _this: &RecordSet<SaleOrder>,
        _tx: &mut Transaction,
    ) -> Result<ProtectedFields<SaleOrder>> {
        Ok(ProtectedFields::new()
            .with(SaleOrder::PARTNER_ID)
            .with(SaleOrder::PARTNER_INVOICE_ID)
            .with(SaleOrder::PARTNER_SHIPPING_ID)
            .with(SaleOrder::COMMITMENT_DATE)
            .with(SaleOrder::ORDER_LINE))
    }

    /// The values a new quotation form starts with: the active company, unless given.
    #[method(override)]
    async fn default_get(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut values: ValueMap = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "company_id")
            && !values.contains_key("company_id")
            && let Some(company) = this.env().company_id()?
        {
            values.insert("company_id".into(), Value::Int(company));
        }
        if names.iter().any(|n| n == "validity_date") && !values.contains_key("validity_date") {
            let days = crate::settings::quotation_validity_days(tx).await?;
            if days > 0 {
                let today = Date::today();
                values.insert(
                    "validity_date".into(),
                    Value::Date(Date::from_days(today.days() + days)),
                );
            }
        }
        Ok(values)
    }

    /// An order is born a draft and unlocked; it takes the current company when none is given, the
    /// next number of its company's sequence when it has no name, and its customer's invoice and
    /// delivery addresses when none are given (B2,
    #[method(override)]
    async fn create(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env().clone();
        // the number is drawn below: the caller's create right is checked before anything is consumed
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        refuse_context_defaults(&env, "sales order", CREATE_REFUSED)?;
        for vals in &vals_list {
            refuse_operation_fields(tx, vals, CREATE_REFUSED)?;
            crate::amounts::refuse_derived(
                tx,
                this.env(),
                vals,
                "sale.order",
                &crate::amounts::ORDER_DERIVED,
            )?;
        }
        let current = env.company_id()?;
        for vals in &mut vals_list {
            if !vals.contains("company_id")
                && let Some(company) = current
            {
                vals.insert("company_id".into(), Cell::Id(company));
            }
            let unnamed = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(name)) => name.as_ref() == PLACEHOLDER,
                Some(_) => false,
            };
            if unnamed {
                let company = match vals.get("company_id") {
                    Some(Cell::Id(company)) => *company,
                    _ => {
                        return Err(Error::validation(
                            "A sales order needs a company before it can be numbered.",
                        ));
                    }
                };
                // the user's date, the one the prices of the order use
                let at = match vals.get("date_order") {
                    Some(Cell::DateTime(date)) => *date,
                    _ => DateTime::now(),
                };
                let date = Some(crate::amounts::order_day(&env, at)?);
                let number = crate::numbering::next_number(tx, this.env(), company, date).await?;
                vals.insert("name".into(), Cell::Str(number.into()));
            }
            fill_addresses(tx, &env, vals).await?;
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// The client-write rules of the header (B2): the operation fields are never
    /// written; a cancelled order is frozen; a locked order refuses its protected fields; a
    /// confirmed order refuses a new customer and a new company, a quotation with lines a new
    /// company.
    #[method(override)]
    async fn write(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        refuse_operation_fields(tx, &vals, &OPERATION_FIELDS)?;
        crate::amounts::refuse_derived(
            tx,
            this.env(),
            &vals,
            "sale.order",
            &crate::amounts::ORDER_DERIVED,
        )?;
        let records = this.as_records();
        if records.is_empty() || vals.is_empty() {
            return sup.call(tx, records, (vals,)).await;
        }
        let written: BTreeSet<&str> = vals
            .iter()
            .map(|(name, _)| name)
            .chain(vals.iter_commands().map(|(name, _)| name))
            .collect();
        let protected: ProtectedFields<SaleOrder> =
            records.call(tx, "_get_protected_fields", ()).await?;
        // the present company of each order, read only when a company is written
        let companies = if written.contains("company_id") {
            RecordSet::<SaleOrder>::try_from(records.clone())?
                .mapped(tx, SaleOrder::COMPANY_ID)
                .await?
        } else {
            Vec::new()
        };
        let new_company = match vals.get("company_id") {
            Some(Cell::Id(id)) => RecId::from_real(*id),
            _ => None,
        };
        for (index, order) in headers(tx, records).await?.into_iter().enumerate() {
            if order.state == "cancel" {
                return Err(Error::validation(format!(
                    "The sales order {} is cancelled and cannot be changed. Set it back to draft first.",
                    order.name
                )));
            }
            if order.locked {
                let refused: Vec<String> = protected
                    .names()
                    .filter(|name| written.contains(name))
                    .map(|name| label(tx, name))
                    .collect::<Result<_>>()?;
                if !refused.is_empty() {
                    return Err(Error::validation(format!(
                        "The sales order {} is locked. These fields cannot be changed: {}.",
                        order.name,
                        refused.join(", ")
                    )));
                }
            }
            if order.state == "sale" && written.contains("partner_id") {
                return Err(Error::validation(format!(
                    "The customer of the sales order {} cannot be changed after it is confirmed.",
                    order.name
                )));
            }
            if companies
                .get(index)
                .is_some_and(|company| *company != new_company)
            {
                if order.state == "sale" {
                    return Err(Error::validation(format!(
                        "The company of the sales order {} cannot be changed after it is confirmed.",
                        order.name
                    )));
                }
                if has_lines(tx, order.id).await? {
                    return Err(Error::validation(format!(
                        "The sales order {} has lines, whose taxes and prices belong to its company, so it cannot move to another company. Remove the lines first, or make the quotation in the other company.",
                        order.name
                    )));
                }
            }
            // the form's read-only rules are the server's rules, refused by name
            let frozen: &[&str] = if order.state == "sale" {
                &["date_order", "validity_date", "fiscal_position_id"]
            } else {
                &[]
            };
            let locked_only: &[&str] = if order.locked { &LOCKED_ONLY } else { &[] };
            let refused: Vec<String> = frozen
                .iter()
                .chain(locked_only)
                .filter(|name| written.contains(**name))
                .map(|name| label(tx, name))
                .collect::<Result<_>>()?;
            if !refused.is_empty() {
                return Err(Error::validation(format!(
                    "The sales order {} is {}. These fields cannot be changed: {}.",
                    order.name,
                    if order.locked { "locked" } else { "confirmed" },
                    refused.join(", ")
                )));
            }
            if order.state == "sale" && written.contains("pricelist_id") {
                return Err(Error::user(format!(
                    "The pricelist of the sales order {} cannot be changed after it is confirmed.",
                    order.name
                )));
            }
        }
        let recurrency = ["pricelist_id", "company_id"]
            .iter()
            .any(|field| written.contains(field));
        let ids: BTreeSet<i64> = records.ids().into_iter().collect();
        // a new pricelist prices every line again (converting currency when needed)
        let order_lines = if written.contains("pricelist_id") {
            let lines = this
                .env()
                .records("sale.order.line")?
                .search(
                    tx,
                    &Domain::condition(
                        "order_id",
                        Operator::In,
                        ids.iter().copied().collect::<Vec<_>>(),
                    )?,
                )
                .await?;
            Some(lines)
        } else {
            None
        };
        sup.call(tx, records, (vals,)).await?;
        if let Some(lines) = order_lines {
            let empty_manual = BTreeSet::new();
            crate::amounts::reprice(tx, this.env(), &lines, &empty_manual, true, false).await?;
        }
        if recurrency {
            // a new currency rounds every line and total again
            crate::amounts::resync_orders(tx, this.env(), &ids).await?;
        }
        Ok(())
    }

    /// Recomputes unit prices on all product lines of draft/sent quotations from the current
    /// pricelist (including multi-currency conversion).
    #[method]
    #[rpc(effect = write)]
    pub async fn action_update_prices(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        let ids: BTreeSet<i64> = this.as_records().ids().into_iter().collect();
        if ids.is_empty() {
            return Ok(());
        }
        let lines = this
            .env()
            .records("sale.order.line")?
            .search(
                tx,
                &Domain::condition(
                    "order_id",
                    Operator::In,
                    ids.iter().copied().collect::<Vec<_>>(),
                )?,
            )
            .await?;
        let empty_manual = BTreeSet::new();
        crate::amounts::reprice(tx, this.env(), &lines, &empty_manual, true, false).await?;
        crate::amounts::resync_orders(tx, this.env(), &ids).await
    }

    /// Only a draft quotation or a cancelled order can be deleted.
    #[method(override)]
    async fn unlink(this: &RecordSet<SaleOrder>, tx: &mut Transaction, sup: Super) -> Result<()> {
        let refused: Vec<String> = headers(tx, this.as_records())
            .await?
            .iter()
            .filter(|order| !matches!(order.state.as_str(), "draft" | "cancel"))
            .map(Header::describe)
            .collect();
        if !refused.is_empty() {
            return Err(Error::validation(format!(
                "Only a quotation or a cancelled order can be deleted; cancel the others first: {}.",
                refused.join(", ")
            )));
        }
        sup.call(tx, this.as_records(), ()).await
    }
}
