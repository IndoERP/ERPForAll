//! `res.partner`: contacts, hierarchies and context-aware labels.
//! Label/search acceptance: and 079;
//! French type-label translation and delegated-user identity remain partial.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::prelude::*;
use erp_sdk::{Context, FieldType, ValueMap};

use crate::partner_hierarchy::{acquire_hierarchy_fence, validate_partner_hierarchy};

// Native create/write derive this hint from their actual inputs and overwrite caller context.
// Create results are rebound to the caller's original environment before returning.
const HIERARCHY_FENCE_CONTEXT_KEY: &str = "__base_partner_hierarchy_fence";

fn carries_child_commands(values: &RawVals) -> bool {
    // This selects serialization only. The ordinary CRUD path validates field/value shapes
    // and permissions before invoking the constraint; all inputs are passed through intact.
    values
        .commands("child_ids")
        .is_some_and(|commands| !commands.is_empty())
        || matches!(values.get("child_ids"), Some(Cell::Ids(ids)) if !ids.is_empty())
}

/// `res.partner.industry`: commercial industry classification.
#[model(
    name = "res.partner.industry",
    description = "Industry",
    order = "name, id"
)]
pub struct ResPartnerIndustry {
    /// Industry name.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Full description.
    #[field(char)]
    pub full_name: Option<Arc<str>>,
    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,
}

/// `res.partner.category`: partner tags hierarchy.
#[model(
    name = "res.partner.category",
    description = "Partner Tag",
    order = "name, id",
    parent_store = true,
    parent_name = "parent_id"
)]
pub struct ResPartnerCategory {
    /// Tag name.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,
    /// Color index.
    #[field(integer)]
    pub color: i32,
    /// Parent tag.
    #[field(
        many2one,
        comodel = "res.partner.category",
        index = true,
        ondelete = "cascade"
    )]
    pub parent_id: Option<RecId>,
    /// Sub-tags.
    #[field(one2many, comodel = "res.partner.category", inverse_name = "parent_id")]
    pub child_ids: Vec<RecId>,
    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Parent path for hierarchical storage.
    #[field(char, index = true)]
    pub parent_path: Option<Arc<str>>,
    /// Root-first ancestor label in the reader's language.
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,
    /// Linked partners.
    #[field(
        many2many,
        comodel = "res.partner",
        relation = "res_partner_res_partner_category_rel",
        column1 = "category_id",
        column2 = "partner_id"
    )]
    pub partner_ids: Vec<RecId>,
}

#[methods(ResPartnerCategory, addon = "base")]
impl ResPartnerCategory {
    /// Fill the optional color only after context and persisted defaults have had priority.
    #[method(override)]
    async fn default_get(
        this: &RecordSet<ResPartnerCategory>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let color_requested = names.iter().any(|name| name == "color");
        let mut values = sup.call(tx, this.as_records(), (names,)).await?;
        if color_requested && !values.contains_key("color") {
            let row = tx
                .query("SELECT floor(random() * 11)::int4 + 1", &[])
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| Error::programming("Partner tag color default returned no row"))?;
            let color: i32 = row.try_get(0).map_err(|error| {
                Error::db("Partner tag color default decode failed").with_source(error)
            })?;
            values.insert("color".into(), Value::Int(i64::from(color)));
        }
        Ok(values)
    }

    /// Refuse a hierarchy deeper than [`MAX_CATEGORY_DEPTH`] before a tag is created.
    #[method(override)]
    async fn create(
        this: &RecordSet<ResPartnerCategory>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let default_parent = match this.env().context().get("default_parent_id") {
            Some(Value::Int(id)) => Some(id),
            _ => None,
        };
        let parents: Vec<i64> = vals_list
            .iter()
            .filter_map(|vals| match vals.get("parent_id") {
                Some(cell) => category_parent(cell),
                None => default_parent,
            })
            .collect();
        check_category_depth(tx, &parents, &[]).await?;
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// Refuse a parent change that would push the moved subtrees past [`MAX_CATEGORY_DEPTH`], and
    /// drop the cached labels of every descendant after a rename (see the label compute).
    #[method(override)]
    async fn write(
        this: &RecordSet<ResPartnerCategory>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if let Some(parent) = vals.get("parent_id").and_then(category_parent) {
            let moved = this.as_records().ids();
            check_category_depth(tx, &[parent], &moved).await?;
        }
        let renamed = vals.contains("name") && !this.is_empty();
        sup.call(tx, this.as_records(), (vals,)).await?;
        if renamed {
            let ancestors = this.as_records().ids();
            let rows = tx
                .query(
                    "SELECT d.id FROM res_partner_category d \
                     JOIN res_partner_category a ON starts_with(d.parent_path, a.parent_path) \
                     WHERE a.id = ANY($1) AND d.id <> a.id",
                    &[&ancestors],
                )
                .await?;
            let mut descendants = Vec::with_capacity(rows.len());
            for row in &rows {
                let raw: i64 = row.try_get(0).map_err(|error| {
                    Error::db("Partner tag descendant decode failed").with_source(error)
                })?;
                descendants.push(RecId::from_real(raw).ok_or_else(|| {
                    Error::programming("Partner tag descendant has an invalid identity")
                })?);
            }
            if !descendants.is_empty() {
                let stale = this.as_records().browse_ids(descendants);
                tx.invalidate_records(&stale, Some(&["display_name"]))
                    .await?;
            }
        }
        Ok(())
    }

    /// Compose the root-first label from `parent_path` (or parent's path for draft tags) and one
    /// batched read of the ancestor names, so no compute nests per level. The ancestors are read with
    /// the reader's own access: a tag hidden by a record rule fails the read loudly instead of being
    /// dropped.
    ///
    /// The label deliberately does not depend on `parent_id.display_name`: the engine would then
    /// prefetch the parent's label before this compute, one nested compute per level. A rename
    /// of an ancestor instead drops the cached descendant labels in [`Self::write`], and a move
    /// changes `parent_path`, which triggers this compute.
    #[compute(
        depends = ["name", "parent_id", "parent_path"],
        depends_context = ["lang"]
    )]
    async fn _compute_display_name(
        this: &RecordSet<ResPartnerCategory>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResPartnerCategory>,
    ) -> Result<()> {
        const MAX_LABEL_BYTES: usize = 1_048_576;
        let paths = this.mapped(tx, ResPartnerCategory::PARENT_PATH).await?;
        let own = this.as_records().rec_ids().to_vec();
        let parent_ids = this.mapped(tx, ResPartnerCategory::PARENT_ID).await?;
        let own_names = this.mapped(tx, ResPartnerCategory::NAME).await?;

        let mut draft_parent_ids: BTreeSet<RecId> = BTreeSet::new();
        for (id, parent_id) in own.iter().zip(&parent_ids) {
            if id.is_new()
                && let Some(parent_id) = parent_id
            {
                if parent_id.is_new() {
                    return Err(Error::user(
                        "Chained draft tag labels are unsupported (a later release)",
                    ));
                }
                draft_parent_ids.insert(*parent_id);
            }
        }

        let parent_paths_map: BTreeMap<RecId, Arc<str>> = if !draft_parent_ids.is_empty() {
            let parent_ids_vec: Vec<RecId> = draft_parent_ids.into_iter().collect();
            let parent_records = this.as_records().browse_ids(parent_ids_vec.iter().copied());
            let parent_typed: RecordSet<ResPartnerCategory> = parent_records.try_into()?;
            let parent_paths = parent_typed
                .mapped(tx, ResPartnerCategory::PARENT_PATH)
                .await?;
            parent_ids_vec
                .into_iter()
                .zip(parent_paths)
                .map(|(pid, path)| {
                    let path = path.ok_or_else(|| {
                        Error::programming("Parent tag label has no stored hierarchy path")
                    })?;
                    Ok((pid, path))
                })
                .collect::<Result<_>>()?
        } else {
            BTreeMap::new()
        };

        let parse_path = |path: &str| -> Result<Vec<RecId>> {
            let mut chain = Vec::new();
            for part in path.split('/').filter(|part| !part.is_empty()) {
                let raw: i64 = part.parse().map_err(|_| {
                    Error::programming("Partner tag hierarchy path holds a non-numeric part")
                })?;
                let ancestor = RecId::from_real(raw).ok_or_else(|| {
                    Error::programming("Partner tag hierarchy path holds an invalid identity")
                })?;
                chain.push(ancestor);
            }
            Ok(chain)
        };

        let mut chains: Vec<(Vec<RecId>, Option<Arc<str>>)> = Vec::with_capacity(own.len());
        let mut wanted: BTreeSet<RecId> = BTreeSet::new();

        for (i, id) in own.iter().enumerate() {
            if id.is_new() {
                let draft_name = own_names[i].clone().unwrap_or_else(|| Arc::from(""));
                match parent_ids[i] {
                    None => {
                        chains.push((Vec::new(), Some(draft_name)));
                    }
                    Some(ref parent_id) => {
                        let parent_path = parent_paths_map.get(parent_id).ok_or_else(|| {
                            Error::programming("Parent tag label has no stored hierarchy path")
                        })?;
                        let parent_chain = parse_path(parent_path)?;
                        if parent_chain.len() + 1 > MAX_CATEGORY_DEPTH {
                            return Err(Error::validation(format!(
                                "Partner tag hierarchy exceeds depth {MAX_CATEGORY_DEPTH}"
                            )));
                        }
                        if parent_chain.last() != Some(parent_id) {
                            return Err(Error::programming(
                                "Parent tag hierarchy path does not end at parent record",
                            ));
                        }
                        wanted.extend(parent_chain.iter().copied());
                        chains.push((parent_chain, Some(draft_name)));
                    }
                }
            } else {
                let path = paths[i].as_deref().ok_or_else(|| {
                    Error::programming("Partner tag label has no stored hierarchy path")
                })?;
                let chain = parse_path(path)?;
                if chain.len() > MAX_CATEGORY_DEPTH {
                    return Err(Error::validation(format!(
                        "Partner tag hierarchy exceeds depth {MAX_CATEGORY_DEPTH}"
                    )));
                }
                if chain.last() != Some(id) {
                    return Err(Error::programming(
                        "Partner tag hierarchy path does not end at its own record",
                    ));
                }
                wanted.extend(chain.iter().copied());
                chains.push((chain, None));
            }
        }

        let ids: Vec<RecId> = wanted.into_iter().collect();
        let records = this.as_records().browse_ids(ids.iter().copied());
        let typed: RecordSet<ResPartnerCategory> = records.try_into()?;
        let names = typed.mapped(tx, ResPartnerCategory::NAME).await?;
        let names: BTreeMap<RecId, Arc<str>> = ids
            .into_iter()
            .zip(names)
            .map(|(id, name)| {
                name.map(|name| (id, name))
                    .ok_or_else(|| Error::programming("Partner tag label has no required name"))
            })
            .collect::<Result<_>>()?;

        for (index, (chain, draft_name)) in chains.iter().enumerate() {
            let mut size = 0_usize;
            match draft_name {
                None => {
                    for (position, id) in chain.iter().enumerate() {
                        let name = names.get(id).ok_or_else(|| {
                            Error::programming("Partner tag ancestor name was not read")
                        })?;
                        let separator = if position == 0 { 0 } else { 3 };
                        size = size
                            .checked_add(separator)
                            .and_then(|size| size.checked_add(name.len()))
                            .filter(|size| *size <= MAX_LABEL_BYTES)
                            .ok_or_else(|| {
                                Error::validation(
                                    "Partner tag label output exceeds the 1048576-byte limit",
                                )
                            })?;
                    }
                    let mut label = String::with_capacity(size);
                    for (position, id) in chain.iter().enumerate() {
                        if position > 0 {
                            label.push_str(" / ");
                        }
                        let name = names.get(id).ok_or_else(|| {
                            Error::programming("Partner tag ancestor name was not read")
                        })?;
                        label.push_str(name);
                    }
                    out.set(
                        index,
                        ResPartnerCategory::DISPLAY_NAME,
                        Some(Arc::from(label)),
                    )?;
                }
                Some(own_name) => {
                    for (position, id) in chain.iter().enumerate() {
                        let name = names.get(id).ok_or_else(|| {
                            Error::programming("Partner tag ancestor name was not read")
                        })?;
                        let separator = if position == 0 { 0 } else { 3 };
                        size = size
                            .checked_add(separator)
                            .and_then(|size| size.checked_add(name.len()))
                            .filter(|size| *size <= MAX_LABEL_BYTES)
                            .ok_or_else(|| {
                                Error::validation(
                                    "Partner tag label output exceeds the 1048576-byte limit",
                                )
                            })?;
                    }
                    let separator = if chain.is_empty() { 0 } else { 3 };
                    size = size
                        .checked_add(separator)
                        .and_then(|size| size.checked_add(own_name.len()))
                        .filter(|size| *size <= MAX_LABEL_BYTES)
                        .ok_or_else(|| {
                            Error::validation(
                                "Partner tag label output exceeds the 1048576-byte limit",
                            )
                        })?;
                    let mut label = String::with_capacity(size);
                    for (position, id) in chain.iter().enumerate() {
                        if position > 0 {
                            label.push_str(" / ");
                        }
                        let name = names.get(id).ok_or_else(|| {
                            Error::programming("Partner tag ancestor name was not read")
                        })?;
                        label.push_str(name);
                    }
                    if !chain.is_empty() {
                        label.push_str(" / ");
                    }
                    label.push_str(own_name);
                    out.set(
                        index,
                        ResPartnerCategory::DISPLAY_NAME,
                        Some(Arc::from(label)),
                    )?;
                }
            }
        }
        Ok(())
    }
}

/// Deepest `res.partner.category` hierarchy admitted anywhere (root is depth one). The base 1.8
/// path migration, tag creation, tag moves and the label read share this single limit.
pub(crate) const MAX_CATEGORY_DEPTH: usize = 256;
const MAX_CATEGORY_DEPTH_I64: i64 = MAX_CATEGORY_DEPTH as i64;

/// The parent identity a create/write cell names; other shapes are left for field admission.
fn category_parent(cell: &Cell) -> Option<i64> {
    match cell {
        Cell::Id(id) => Some(*id),
        Cell::Int(id) => Some(i64::from(*id)),
        _ => None,
    }
}

/// Refuse a tag hierarchy that would exceed [`MAX_CATEGORY_DEPTH`] after attaching `moved`
/// subtrees (a fresh tag counts as one level) below any of `parents`. One set-based statement
/// walks the parent chains and the moved subtrees, both bounded by the limit, so a cycle or a
/// corrupt deep chain cannot loop. A move under the moved record's own subtree is left for the
/// parent-store loop refusal, which names the loop.
async fn check_category_depth(tx: &mut Transaction, parents: &[i64], moved: &[i64]) -> Result<()> {
    if parents.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let bound = i32::try_from(MAX_CATEGORY_DEPTH + 1)
        .map_err(|_| Error::programming("Partner tag depth limit exceeds int4"))?;
    let rows = tx
        .query(
            "WITH RECURSIVE up(id, depth) AS ( \
             SELECT id, 1 FROM res_partner_category WHERE id = ANY($1) \
             UNION ALL \
             SELECT t.parent_id, up.depth + 1 FROM up \
             JOIN res_partner_category t ON t.id = up.id \
             WHERE t.parent_id IS NOT NULL AND up.depth <= $3::int4 \
             ), down(id, depth) AS ( \
             SELECT id, 1 FROM res_partner_category WHERE id = ANY($2) \
             UNION ALL \
             SELECT c.id, down.depth + 1 FROM down \
             JOIN res_partner_category c ON c.parent_id = down.id \
             WHERE down.depth <= $3::int4 \
             ) SELECT COALESCE((SELECT max(depth) FROM up), 0)::int4, \
             COALESCE((SELECT max(depth) FROM down), 0)::int4, \
             COALESCE((SELECT bool_or(id = ANY($2)) FROM up), false)",
            &[&parents, &moved, &bound],
        )
        .await?;
    let [row] = rows.as_slice() else {
        return Err(Error::programming(
            "Partner tag depth check expected one summary row",
        ));
    };
    let decode = |error| Error::db("Partner tag depth check decode failed").with_source(error);
    let parent_depth: i32 = row.try_get(0).map_err(decode)?;
    let moved_height: i32 = row.try_get(1).map_err(decode)?;
    let loops: bool = row.try_get(2).map_err(decode)?;
    if loops {
        return Ok(());
    }
    let total = i64::from(parent_depth) + i64::from(moved_height.max(1));
    if total > MAX_CATEGORY_DEPTH_I64 {
        return Err(Error::validation(format!(
            "Partner tag hierarchy exceeds depth {MAX_CATEGORY_DEPTH}"
        )));
    }
    Ok(())
}

/// `res.partner`: contacts, addresses and hierarchies.
#[model(
    name = "res.partner",
    company_access = direct(field = "company_id", branch = "ancestor", empty = "allow", escape = "partner_share"),
    description = "Contact",
    order = "complete_name, id desc",
    rec_names_search = ["complete_name", "email", "ref", "vat"],
    constraints = [(
        "_name_required_for_contact",
        "CHECK(type != 'contact' OR name IS NOT NULL)",
        "Contacts require a name"
    )]
)]
pub struct ResPartner {
    /// Contact name. Required for `contact` type only.
    #[field(char, index = true)]
    pub name: Option<Arc<str>>,
    /// Hierarchical complete name.
    #[field(char, index = true, compute = "_compute_complete_name", store = true)]
    pub complete_name: Option<Arc<str>>,
    /// Context-dependent display name.
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,
    /// Parent contact / company.
    #[field(many2one, comodel = "res.partner", index = true, ondelete = "set null")]
    pub parent_id: Option<RecId>,
    /// Related parent name.
    #[field(char, related = "parent_id.name")]
    pub parent_name: Option<Arc<str>>,
    /// Active children.
    #[field(
        one2many,
        comodel = "res.partner",
        inverse_name = "parent_id",
        domain = [("active", "=", true)]
    )]
    pub child_ids: Vec<RecId>,
    /// Internal reference.
    #[field(char, index = true, string = "Reference")]
    pub r#ref: Option<Arc<str>>,
    /// Language code.
    #[field(
        selection,
        selection_method = "_lang_get",
        compute = "_compute_lang",
        store = true,
        readonly = false,
        compute_sudo = false
    )]
    pub lang: Option<Arc<str>>,
    /// Time zone name.
    #[field(selection, selection_method = "_tz_get")]
    pub tz: Option<Arc<str>>,
    /// Current UTC offset of the time zone.
    #[field(char, compute = "_compute_tz_offset")]
    pub tz_offset: Option<Arc<str>>,
    /// Tax ID (VAT).
    #[field(char, index = true, string = "Tax ID")]
    pub vat: Option<Arc<str>>,
    /// True when vat is set and valid.
    #[field(
        boolean,
        compute = "_compute_has_vat",
        search_method = "_search_has_vat",
        store = false
    )]
    pub has_vat: bool,
    /// Website URL.
    #[field(char)]
    pub website: Option<Arc<str>>,
    /// Partner tags.
    #[field(
        many2many,
        comodel = "res.partner.category",
        relation = "res_partner_res_partner_category_rel",
        column1 = "partner_id",
        column2 = "category_id"
    )]
    pub category_id: Vec<RecId>,
    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Address type: contact, invoice, delivery, other.
    #[field(
        selection,
        selection = [
            ("contact", "Contact"),
            ("invoice", "Invoice"),
            ("delivery", "Delivery"),
            ("other", "Other")
        ],
        default = "contact"
    )]
    pub r#type: Option<Arc<str>>,
    /// Street line 1.
    #[field(char)]
    pub street: Option<Arc<str>>,
    /// Street line 2.
    #[field(char, string = "Street 2")]
    pub street2: Option<Arc<str>>,
    /// Postal code.
    #[field(char, string = "ZIP")]
    pub zip: Option<Arc<str>>,
    /// City name.
    #[field(char)]
    pub city: Option<Arc<str>>,
    /// State of the address.
    #[field(many2one, comodel = "res.country.state", ondelete = "restrict")]
    pub state_id: Option<RecId>,
    /// Country of the address.
    #[field(many2one, comodel = "res.country", ondelete = "restrict")]
    pub country_id: Option<RecId>,
    /// Country code (ISO 2 chars).
    #[field(char, related = "country_id.code", readonly = true)]
    pub country_code: Option<Arc<str>>,
    /// Contact email.
    #[field(char)]
    pub email: Option<Arc<str>>,
    /// Formatted email ("Name" <email>).
    #[field(char, compute = "_compute_email_formatted")]
    pub email_formatted: Option<Arc<str>>,
    /// Contact phone.
    #[field(char)]
    pub phone: Option<Arc<str>>,
    /// True if commercial partner and has VAT.
    #[field(
        boolean,
        compute = "_compute_is_company",
        store = true,
        readonly = true
    )]
    pub is_company: bool,
    /// Industry classification.
    #[field(many2one, comodel = "res.partner.industry")]
    pub industry_id: Option<RecId>,
    /// Owning company.
    #[field(many2one, comodel = "res.company", ondelete = "restrict", index = true)]
    pub company_id: Option<RecId>,
    /// Color index.
    #[field(integer, default = 0)]
    pub color: i32,
    /// Linked users (archived ones are hidden on read unless `active_test` is off).
    #[field(one2many, comodel = "res.users", inverse_name = "partner_id")]
    pub user_ids: Vec<RecId>,
    /// Main user for authentication/ownership.
    #[field(many2one, comodel = "res.users", compute = "_compute_main_user_id")]
    pub main_user_id: Option<RecId>,
    /// True unless the contact is the superuser's partner or has an active internal user;
    /// the escape of the company restriction. Required with
    /// the static default `true`, so a row that was never computed takes the company test (fail
    /// closed); the stored compute replaces the default after the insert, since a computed field
    /// without inverse is not protected by a given value. The default is also the column's
    /// database default (`db_default`) for rows written outside the ORM.
    #[field(
        boolean,
        compute = "_compute_partner_share",
        store = true,
        required,
        default = true,
        db_default
    )]
    pub partner_share: bool,
    /// Topmost ancestor in hierarchy.
    #[field(
        many2one,
        comodel = "res.partner",
        index = true,
        compute = "_compute_commercial_partner",
        store = true,
        precompute = false
    )]
    pub commercial_partner_id: Option<RecId>,
    /// Related name of topmost ancestor.
    #[field(char, related = "commercial_partner_id.name", store = true)]
    pub commercial_company_name: Option<Arc<str>>,
}

fn compute_tz_offset(tz_name: Option<&str>) -> Result<Arc<str>> {
    let offset_seconds = match tz_name {
        Some("") | None => 0,
        Some(name) => {
            let zone = jiff::tz::TimeZone::get(name)
                .map_err(|_| Error::validation("res.partner.tz: unknown timezone"))?;
            zone.to_offset(jiff::Timestamp::now()).seconds()
        }
    };
    let sign = if offset_seconds >= 0 { '+' } else { '-' };
    let total_minutes = offset_seconds.abs() / 60;
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    Ok(Arc::from(format!("{sign}{hours:02}{minutes:02}")))
}

fn normalise_website(val: Option<&str>) -> Option<Arc<str>> {
    let s = val?.trim();
    if s.is_empty() {
        return None;
    }
    if s.starts_with("http://") || s.starts_with("https://") || s.starts_with("ftp://") {
        Some(Arc::from(s))
    } else {
        Some(Arc::from(format!("http://{s}")))
    }
}

fn normalise_website_cell(cell: &Cell) -> Result<Cell> {
    let text = match cell {
        Cell::Str(text) => Some(text.as_ref()),
        Cell::Null => None,
        _ => {
            return Err(Error::programming(
                "res.partner.website: expected a string or null",
            ));
        }
    };
    Ok(match normalise_website(text) {
        Some(text) => Cell::Str(text),
        None => Cell::Null,
    })
}

fn context_flag(this: &RecordSet<ResPartner>, key: &str) -> bool {
    // These are the specified truthy display/name-create context flags, not typed field inputs.
    this.env()
        .context()
        .get(key)
        .is_some_and(|value| value.is_truthy())
}

fn static_type_labels(this: &RecordSet<ResPartner>) -> Result<BTreeMap<String, Arc<str>>> {
    let field = this
        .as_records()
        .spec()
        .field("type")
        .ok_or_else(|| Error::programming("res.partner.type: selection field missing"))?;
    match &field.ty {
        FieldType::Selection {
            values: Some(values),
            ..
        } => Ok(values
            .iter()
            .map(|(key, label)| (key.clone(), Arc::from(label.as_str())))
            .collect()),
        _ => Err(Error::programming(
            "res.partner.type: expected static selection labels",
        )),
    }
}

async fn type_labels(
    this: &RecordSet<ResPartner>,
    tx: &mut Transaction,
) -> Result<BTreeMap<String, Arc<str>>> {
    let lang = this.env().lang().unwrap_or("en_US");
    let pairs = tx
        .selection_labels_for_lang("res.partner", "type", lang)
        .await?;
    Ok(pairs
        .into_iter()
        .map(|(key, label)| (key, Arc::from(label.as_str())))
        .collect())
}

async fn displayed_type_keys(
    this: &RecordSet<ResPartner>,
    tx: &mut Transaction,
    labels: &BTreeMap<String, Arc<str>>,
) -> Result<BTreeSet<String>> {
    let keys = this
        .as_records()
        .call::<(), Vec<String>>(tx, "_complete_name_displayed_types", ())
        .await?;
    let mut displayed = BTreeSet::new();
    for key in keys {
        if !labels.contains_key(&key) {
            return Err(Error::programming(format!(
                "res.partner._complete_name_displayed_types: unknown type '{key}'"
            )));
        }
        if !displayed.insert(key.clone()) {
            return Err(Error::programming(format!(
                "res.partner._complete_name_displayed_types: duplicate type '{key}'"
            )));
        }
    }
    Ok(displayed)
}

async fn parent_names(
    this: &RecordSet<ResPartner>,
    tx: &mut Transaction,
    parents: &[Option<RecId>],
    sudo: bool,
) -> Result<BTreeMap<RecId, Option<Arc<str>>>> {
    let ids: BTreeSet<RecId> = parents.iter().flatten().copied().collect();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let records = this.as_records().browse_ids(ids.iter().copied());
    // Only the complete-name parent fallback uses sudo, as required by PARTNER §4.3.
    // Formatted display reads preserve the ordinary compute environment.
    let records = if sudo { records.sudo(true) } else { records };
    let typed: RecordSet<ResPartner> = records.try_into()?;
    let names = typed.mapped(tx, ResPartner::NAME).await?;
    Ok(ids.into_iter().zip(names).collect())
}

async fn complete_names(
    this: &RecordSet<ResPartner>,
    tx: &mut Transaction,
) -> Result<Vec<Arc<str>>> {
    let labels = this
        .as_records()
        .call::<(), Vec<Arc<str>>>(tx, "_get_complete_name", ())
        .await?;
    if labels.len() != this.len() {
        return Err(Error::programming(format!(
            "res.partner._get_complete_name: expected {} labels, got {}",
            this.len(),
            labels.len()
        )));
    }
    for label in &labels {
        crate::partner_labels::check_label(label)?;
    }
    Ok(labels)
}

#[methods(ResPartner, addon = "base")]
impl ResPartner {
    /// Return installed languages' (code, name) pairs sorted by name.
    #[selection]
    async fn _lang_get(
        _this: &RecordSet<ResPartner>,
        ctx: &mut erp_sdk::SelectionContext<'_>,
    ) -> Result<()> {
        let domain = Domain::condition("active", Operator::Eq, true)?;
        ctx.catalog("res.lang", &domain, "code", "name", "name, id")
            .await
    }

    /// Return timezone options with `Etc/*` sorted last.
    #[selection]
    async fn _tz_get(
        _this: &RecordSet<ResPartner>,
        ctx: &mut erp_sdk::SelectionContext<'_>,
    ) -> Result<()> {
        let mut tzs = Vec::new();
        for name in jiff::tz::db().available() {
            ctx.charge(1)?;
            tzs.push(name.as_str().to_owned());
        }
        tzs.sort_by(|a, b| {
            let a_etc = a.starts_with("Etc/");
            let b_etc = b.starts_with("Etc/");
            match (a_etc, b_etc) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => a.cmp(b),
            }
        });
        for tz in tzs {
            ctx.emit(&tz, &tz)?;
        }
        Ok(())
    }

    /// Contacts require a non-empty name.
    #[constrains("name", "type")]
    async fn _check_contact_name(this: &RecordSet<ResPartner>, tx: &mut Transaction) -> Result<()> {
        let ptypes = this.mapped(tx, ResPartner::TYPE).await?;
        let names = this.mapped(tx, ResPartner::NAME).await?;
        for (ptype, name) in ptypes.into_iter().zip(names) {
            let is_contact = ptype.as_deref().unwrap_or("contact") == "contact";
            if is_contact && name.as_deref().map(str::trim).unwrap_or("").is_empty() {
                return Err(Error::validation("Contacts require a name"));
            }
        }
        Ok(())
    }

    /// Refuse individually oversized label inputs within ordinary CRUD validation.
    #[constrains("street", "street2", "city", "zip", "vat", "email")]
    async fn _check_label_inputs(this: &RecordSet<ResPartner>, tx: &mut Transaction) -> Result<()> {
        for (field, values) in [
            ("street", this.mapped(tx, ResPartner::STREET).await?),
            ("street2", this.mapped(tx, ResPartner::STREET2).await?),
            ("city", this.mapped(tx, ResPartner::CITY).await?),
            ("zip", this.mapped(tx, ResPartner::ZIP).await?),
            ("vat", this.mapped(tx, ResPartner::VAT).await?),
        ] {
            for value in values.into_iter().flatten() {
                crate::partner_labels::check_input(field, &value)?;
            }
        }
        for email in this
            .mapped(tx, ResPartner::EMAIL)
            .await?
            .into_iter()
            .flatten()
        {
            crate::partner_email::validate_input(&email)?;
        }
        Ok(())
    }

    /// Prevent cycles in partner hierarchy.
    #[constrains("parent_id", "child_ids")]
    async fn _check_parent_recursion(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
    ) -> Result<()> {
        if this.is_empty() {
            return Ok(());
        }
        let fence = match this.env().context().get(HIERARCHY_FENCE_CONTEXT_KEY) {
            Some(Value::Bool(fence)) => fence,
            None => false, // A direct check does not itself mutate the hierarchy.
            Some(_) => {
                return Err(Error::programming(
                    "internal hierarchy operation hint must be a boolean",
                ));
            }
        };
        if fence {
            // Native constraints run after write model, record and field authorization.
            // Create model/field checks precede this; final create record rules run later.
            acquire_hierarchy_fence(tx).await?;
        }
        let parents = this.mapped(tx, ResPartner::PARENT_ID).await?;
        if parents.iter().all(Option::is_none) {
            return Ok(());
        }
        let pairs: Vec<(i64, Option<i64>)> = this
            .ids()
            .into_iter()
            .zip(parents)
            .map(|(id, p)| (id, p.map(|r| r.get())))
            .collect();
        validate_partner_hierarchy(tx, &pairs, this.env().context()).await
    }

    #[compute(depends = ["vat"])]
    fn _compute_has_vat(batch: &mut Batch<'_, ResPartner>) -> Result<()> {
        for i in batch.indices() {
            let vat = batch.get(i, ResPartner::VAT)?;
            let has = match vat.as_deref() {
                Some(s) => !s.is_empty() && s != "/" && s != "na" && s != "NA",
                None => false,
            };
            batch.set(i, ResPartner::HAS_VAT, has)?;
        }
        Ok(())
    }

    #[compute(depends = ["parent_id.commercial_partner_id"])]
    fn _compute_commercial_partner(batch: &mut Batch<'_, ResPartner>) -> Result<()> {
        for i in batch.indices() {
            let parent = batch.get(i, ResPartner::PARENT_ID)?;
            let comm_id = match parent {
                Some(p) => batch.value_of(ResPartner::COMMERCIAL_PARTNER_ID, p)?,
                None => Some(batch.ids()[i]),
            };
            batch.set(i, ResPartner::COMMERCIAL_PARTNER_ID, comm_id)?;
        }
        Ok(())
    }

    #[compute(depends = ["has_vat", "commercial_partner_id"])]
    fn _compute_is_company(batch: &mut Batch<'_, ResPartner>) -> Result<()> {
        for i in batch.indices() {
            let has_vat = batch.get(i, ResPartner::HAS_VAT)?;
            let comm = batch.get(i, ResPartner::COMMERCIAL_PARTNER_ID)?;
            let is_self = comm == Some(batch.ids()[i]);
            batch.set(i, ResPartner::IS_COMPANY, is_self && has_vat)?;
        }
        Ok(())
    }

    /// Address types that substitute their source label for a missing contact name.
    /// Native overrides may extend this list; consumers validate all returned keys.
    #[method]
    async fn _complete_name_displayed_types(
        _this: &RecordSet<ResPartner>,
        _tx: &mut Transaction,
    ) -> Result<Vec<String>> {
        Ok(vec!["invoice".into(), "delivery".into(), "other".into()])
    }

    /// Compose one complete name per input record, in recordset order.
    /// Uses translated selection labels for context language.
    #[method]
    async fn _get_complete_name(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
    ) -> Result<Vec<Arc<str>>> {
        let labels = type_labels(this, tx).await?;
        let displayed = displayed_type_keys(this, tx, &labels).await?;
        let names = this.mapped(tx, ResPartner::NAME).await?;
        let parents = this.mapped(tx, ResPartner::PARENT_ID).await?;
        let types = this.mapped(tx, ResPartner::TYPE).await?;
        let companies = this.mapped(tx, ResPartner::IS_COMPANY).await?;
        let commercial = this.mapped(tx, ResPartner::COMMERCIAL_COMPANY_NAME).await?;
        let parent_names = parent_names(this, tx, &parents, true).await?;
        let hide_company = context_flag(this, "partner_display_name_hide_company");
        let mut output = Vec::with_capacity(this.len());
        for index in 0..this.len() {
            // Missing optional names are empty by the PARTNER complete-name contract.
            let name = names[index].as_deref().unwrap_or("");
            let type_key = types[index].as_deref().unwrap_or("contact");
            let type_label = if displayed.contains(type_key) {
                labels.get(type_key).map(AsRef::as_ref)
            } else {
                None
            };
            let parent_name = parents[index].map(|id| {
                parent_names
                    .get(&id)
                    .and_then(Option::as_deref)
                    .unwrap_or("")
            });
            let label =
                crate::partner_labels::complete_name(crate::partner_labels::CompleteName {
                    name,
                    parent: parent_name,
                    commercial_name: commercial[index].as_deref(),
                    is_company: companies[index],
                    type_label,
                    hide_company,
                })?;
            output.push(Arc::from(label));
        }
        Ok(output)
    }

    #[compute(depends = [
        "is_company", "name", "parent_id.name", "type", "commercial_company_name"
    ])]
    async fn _compute_complete_name(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResPartner>,
    ) -> Result<()> {
        let source = this.with_env(this.env().with_context_replaced(Context::new()));
        let labels = complete_names(&source, tx).await?;
        for (index, label) in labels.into_iter().enumerate() {
            out.set(index, ResPartner::COMPLETE_NAME, Some(label))?;
        }
        Ok(())
    }

    #[compute(
        depends = [
            "complete_name", "name", "parent_id.name", "is_company", "type",
            "commercial_company_name", "email", "vat", "street", "street2", "city", "zip"
        ],
        depends_context = [
            "show_address", "partner_show_db_id", "show_email", "show_identifier", "lang",
            "formatted_display_name", "partner_display_name_hide_company"
        ]
    )]
    async fn _compute_display_name(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResPartner>,
    ) -> Result<()> {
        use crate::partner_labels::{Address, Label};
        let formatted = context_flag(this, "formatted_display_name");
        let show_email = context_flag(this, "show_email");
        let show_id = context_flag(this, "partner_show_db_id");
        let show_address = context_flag(this, "show_address");
        let show_identifier = context_flag(this, "show_identifier");
        let complete = if formatted {
            Vec::new()
        } else {
            complete_names(this, tx).await?
        };
        let names = this.mapped(tx, ResPartner::NAME).await?;
        let parents = this.mapped(tx, ResPartner::PARENT_ID).await?;
        let types = this.mapped(tx, ResPartner::TYPE).await?;
        let emails = this.mapped(tx, ResPartner::EMAIL).await?;
        let vats = this.mapped(tx, ResPartner::VAT).await?;
        let streets = this.mapped(tx, ResPartner::STREET).await?;
        let streets2 = this.mapped(tx, ResPartner::STREET2).await?;
        let cities = this.mapped(tx, ResPartner::CITY).await?;
        let zips = this.mapped(tx, ResPartner::ZIP).await?;
        let parent_names = if formatted {
            parent_names(this, tx, &parents, false).await?
        } else {
            BTreeMap::new()
        };
        let type_labels = type_labels(this, tx).await?;
        for index in 0..this.len() {
            let mut label = Label::default();
            let email = emails[index].as_deref().filter(|email| !email.is_empty());
            if formatted {
                // Own optional name and a nameless parent's name are empty by §4.3.
                let own = names[index].as_deref().unwrap_or("");
                if let Some(parent) = parents[index] {
                    label.push(
                        parent_names
                            .get(&parent)
                            .and_then(Option::as_deref)
                            .unwrap_or(""),
                    )?;
                    label.push(" \t --")?;
                    let type_label = types[index]
                        .as_deref()
                        .and_then(|key| type_labels.get(key))
                        .map(AsRef::as_ref);
                    label.push(if own.is_empty() {
                        type_label.unwrap_or("")
                    } else {
                        own
                    })?;
                    label.push("--")?;
                } else {
                    label.push(own)?;
                }
                if show_email && let Some(email) = email {
                    label.push(" \t --")?;
                    label.push(email)?;
                    label.push("--")?;
                } else if show_id {
                    label.push(" \t --")?;
                    label.push(&this.as_records().rec_ids()[index].get().to_string())?;
                    label.push("--")?;
                }
            } else {
                label.push(&complete[index])?;
                if show_id {
                    label.push(" (")?;
                    label.push(&this.as_records().rec_ids()[index].get().to_string())?;
                    label.push(")")?;
                }
                if show_email && let Some(email) = email {
                    label.push(" <")?;
                    label.push(email)?;
                    label.push(">")?;
                }
                if show_address {
                    let address = crate::partner_labels::address(Address {
                        street: streets[index].as_deref(),
                        street2: streets2[index].as_deref(),
                        city: cities[index].as_deref(),
                        zip: zips[index].as_deref(),
                    })?;
                    label.push("\n")?;
                    label.push(&address)?;
                }
                if show_identifier
                    && let Some(vat) = vats[index].as_deref().filter(|vat| !vat.is_empty())
                {
                    label.push(if show_address { "\n " } else { " - " })?;
                    label.push(vat)?;
                }
            }
            out.set(
                index,
                ResPartner::DISPLAY_NAME,
                Some(Arc::from(label.finish_display())),
            )?;
        }
        Ok(())
    }

    #[compute(
        depends = ["parent_id"],
        reads = ["lang", "parent_id.lang"]
    )]
    async fn _compute_lang(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResPartner>,
    ) -> Result<()> {
        let preferred = this
            .env()
            .context()
            .get("default_lang")
            .and_then(|value| match value {
                Value::Str(value) => Some(value),
                _ => None,
            })
            .or_else(|| this.env().lang().map(Arc::from));
        // Own old values and parent values are declared reads, prefetched by the compute
        // engine. Resolve the catalog only after explicit/context/inherited values fail.
        let current = this.mapped(tx, ResPartner::LANG).await?;
        let parents = this.mapped(tx, ResPartner::PARENT_ID).await?;
        let mut installed: Option<Option<Arc<str>>> = None;
        for (i, current) in current.into_iter().enumerate() {
            let inherited = if current.is_none() {
                match parents[i] {
                    Some(parent) => {
                        this.browse([parent.get()])?
                            .get(tx, ResPartner::LANG)
                            .await?
                    }
                    None => None,
                }
            } else {
                None
            };
            let mut language = current.or(inherited).or_else(|| preferred.clone());
            if language.is_none() {
                if installed.is_none() {
                    let languages = this
                        .env()
                        .records("res.lang")?
                        .search_read(
                            tx,
                            &Domain::condition("active", Operator::Eq, true)?,
                            &["code"],
                            &SearchOptions::new().order("id").limit(1),
                        )
                        .await?;
                    let code = match languages.first() {
                        Some(language) => match language.get("code") {
                            Some(Value::Str(code)) => Some(code.clone()),
                            _ => {
                                return Err(Error::programming("res.lang.code: expected a string"));
                            }
                        },
                        None => None,
                    };
                    installed = Some(code);
                }
                if let Some(code) = &installed {
                    language = code.clone();
                }
            }
            out.set(i, ResPartner::LANG, language)?;
        }
        Ok(())
    }

    #[compute(depends = ["tz"])]
    fn _compute_tz_offset(batch: &mut Batch<'_, ResPartner>) -> Result<()> {
        for i in batch.indices() {
            let tz = batch.get(i, ResPartner::TZ)?;
            let offset = compute_tz_offset(tz.as_deref())?;
            batch.set(i, ResPartner::TZ_OFFSET, Some(offset))?;
        }
        Ok(())
    }

    #[compute(depends = ["name", "email"])]
    fn _compute_email_formatted(batch: &mut Batch<'_, ResPartner>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, ResPartner::NAME)?;
            let email = batch.get(i, ResPartner::EMAIL)?;
            let formatted = crate::partner_email::format_email(name.as_deref(), email.as_deref())?;
            batch.set(i, ResPartner::EMAIL_FORMATTED, formatted)?;
        }
        Ok(())
    }

    /// Create through ordinary create dispatch after bounded contact-text parsing.
    /// Validation refuses force_email without a parsed mailbox; normal create errors propagate.
    #[method(override)]
    async fn name_create(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        text: String,
        _sup: Super,
    ) -> Result<(i64, Arc<str>)> {
        let (name, email) = crate::partner_email::parse_contact(&text)?;
        if context_flag(this, "force_email") && email.is_none() {
            return Err(Error::validation(
                "Couldn't create contact without email address!",
            ));
        }
        let labels = static_type_labels(this)?;
        let mut context = this.env().context().clone();
        if let Some(value) = context.get("default_type")
            && value.is_truthy()
        {
            let valid = match value {
                Value::Str(value) => labels.contains_key(value.as_ref()),
                _ => false, // Only truthy invalid defaults are removed.
            };
            if !valid {
                context.remove("default_type");
            }
        }
        let model = this.with_env(this.env().with_context_replaced(context));
        let mut values = RawVals::new().set_cell("name", Cell::Str(name.into()));
        if let Some(email) = email {
            values = values.set_cell("email", Cell::Str(email.into()));
        }
        let created = model.as_records().create(tx, vec![values]).await?;
        let id = match created.ids().as_slice() {
            [id] => *id,
            _ => {
                return Err(Error::programming(
                    "res.partner.name_create: expected one created record",
                ));
            }
        };
        let display = match created.value(tx, "display_name").await? {
            Value::Str(label) => label,
            _ => {
                return Err(Error::programming(
                    "res.partner.name_create: expected a string display_name",
                ));
            }
        };
        Ok((id, display))
    }

    #[compute(depends = [])]
    fn _compute_main_user_id(batch: &mut Batch<'_, ResPartner>) -> Result<()> {
        for i in batch.indices() {
            let none_user: Option<RecId> = None;
            batch.set(i, ResPartner::MAIN_USER_ID, none_user)?;
        }
        Ok(())
    }

    /// archived or not) and for a partner with an active user whose `share` is false; `true`
    /// otherwise. The users are read with `active_test` off (the cache of `user_ids` holds the
    /// archived ones too), then filtered here, so the caller's context cannot change the rule.
    ///
    /// The dependencies fire for a change of `share` or `active` of a linked user (the trigger
    /// tree through `res.users.partner_id`) and for a change of `res.users.partner_id` itself,
    /// which notifies the old and the new partner through the inverse one2many (REL §4.4.4).
    /// The set-based path behind group changes applies the same rule
    /// ([`crate::group_graph::recompute_user_share`]).
    #[compute(depends = ["user_ids.share", "user_ids.active"])]
    async fn _compute_partner_share(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResPartner>,
    ) -> Result<()> {
        let partners = this
            .as_records()
            .sudo(true)
            .with_context([("active_test", Value::Bool(false))]);
        let linked = partners.cells(tx, "user_ids").await?;
        let mut per_partner: Vec<Vec<RecId>> = Vec::with_capacity(linked.len());
        let mut users: Vec<RecId> = Vec::new();
        let mut seen: BTreeSet<RecId> = BTreeSet::new();
        for cell in &linked {
            let ids: Vec<RecId> = match cell {
                Cell::Ids(ids) => ids.to_vec(),
                // a new partner without users
                Cell::Null => Vec::new(),
                other => {
                    return Err(Error::programming(format!(
                        "res.partner.user_ids holds {other:?}, expected a list of user ids"
                    )));
                }
            };
            users.extend(ids.iter().copied().filter(|id| seen.insert(*id)));
            per_partner.push(ids);
        }
        let users = partners.env().records("res.users")?.browse_ids(users);
        let actives = users.cells(tx, "active").await?;
        let shares = users.cells(tx, "share").await?;
        let mut internal: BTreeSet<RecId> = BTreeSet::new();
        for ((id, active), share) in users.rec_ids().iter().zip(&actives).zip(&shares) {
            let superuser = !id.is_new() && id.get() == erp_sdk::SUPERUSER_ID.get();
            let active = match active {
                Cell::Bool(active) => *active,
                Cell::Null => false,
                other => {
                    return Err(Error::programming(format!(
                        "res.users.active holds {other:?}, expected a boolean"
                    )));
                }
            };
            let share = match share {
                Cell::Bool(share) => *share,
                other => {
                    return Err(Error::programming(format!(
                        "res.users.share of user {id} holds {other:?}, expected a boolean (the \
                         column is required)"
                    )));
                }
            };
            if superuser || (active && !share) {
                internal.insert(*id);
            }
        }
        for (i, ids) in per_partner.iter().enumerate() {
            let share = !ids.iter().any(|id| internal.contains(id));
            out.set(i, ResPartner::PARTNER_SHARE, share)?;
        }
        Ok(())
    }

    #[method(override)]
    async fn default_get(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let requested = names.clone();
        let ctx = this.env().context();
        let this_sanitized = if let Some(Value::Str(t)) = ctx.get("default_type") {
            if !matches!(t.as_ref(), "contact" | "invoice" | "delivery" | "other") {
                this.with_context([("default_type".to_string(), Value::False)])
            } else {
                this.clone()
            }
        } else {
            this.clone()
        };
        let mut out = sup.call(tx, this_sanitized.as_records(), (names,)).await?;

        // Check default parent company inheritance
        let parent_candidate: Option<i64> = out
            .get("parent_id")
            .and_then(|v| match v {
                Value::Int(id) => Some(*id),
                _ => None,
            })
            .or_else(|| {
                ctx.get("default_parent_id").and_then(|v| match v {
                    Value::Int(id) => Some(id),
                    _ => None,
                })
            });

        if requested.iter().any(|name| name == "company_id")
            && let Some(pid) = parent_candidate
        {
            let parent = this.browse([pid])?;
            if let Some(comp_id) = parent.get(tx, ResPartner::COMPANY_ID).await? {
                out.insert("company_id".into(), Value::Int(comp_id.get()));
            }
        }

        // Drop/clear invalid default_type
        if let Some(t) = out.get("type") {
            let valid = match t {
                Value::Str(s) => matches!(s.as_ref(), "contact" | "invoice" | "delivery" | "other"),
                _ => false,
            };
            if !valid {
                out.shift_remove("type");
            }
        }

        // Apply tz from context if requested
        if requested.iter().any(|name| name == "tz")
            && !out.contains_key("tz")
            && let Some(Value::Str(tz)) = ctx.get("tz")
        {
            out.insert("tz".into(), Value::Str(tz.clone()));
        }

        // PARTNER-072: the Contacts category filter is a field default only when
        // neither default_category_id nor a persisted default supplied a value.
        // Serialize through the ordinary SET command; create still applies the
        // standard target-access, identity and relation checks.
        if requested.iter().any(|name| name == "category_id")
            && !out.contains_key("category_id")
            && let Some(category) = ctx.get("category_id")
        {
            let invalid = || {
                Error::validation(
                    "res.partner.category_id: context category_id must be a positive integer or false",
                )
            };
            let ids = match category {
                Value::Int(id) => vec![RecId::from_real(id).ok_or_else(invalid)?],
                Value::False | Value::Bool(false) => Vec::new(),
                _ => return Err(invalid()),
            };
            out.insert(
                "category_id".to_string(),
                Value::list([Command::Set(ids).to_value()]),
            );
        }

        out.retain(|name, _| requested.contains(name));
        Ok(out)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        if vals_list.is_empty() {
            return sup.call(tx, this.as_records(), (vals_list,)).await;
        }
        let guard = crate::partner_address_sync::OperationGuard::enter(tx, this.as_records())?;
        if vals_list.len() > 4096 {
            return Err(Error::validation(
                "Partner address maximum visited nodes limit of 4096 exceeded.",
            ));
        }
        let ctx = this.env().context();
        let has_child_commands = vals_list.iter().any(carries_child_commands)
            || ctx
                .get("default_child_ids")
                .is_some_and(|value| value.is_truthy());
        for vals in &mut vals_list {
            // Apply context default_lang if not provided
            if !vals.contains("lang")
                && let Some(Value::Str(l)) = ctx.get("default_lang")
            {
                vals.insert("lang".into(), Cell::Str(l.clone()));
            }

            // Apply context tz default if not provided
            if !vals.contains("tz")
                && let Some(Value::Str(tz)) = ctx.get("tz")
            {
                vals.insert("tz".into(), Cell::Str(tz.clone()));
            }

            // Normalise website
            if let Some(cell) = vals.get("website") {
                let normalised = normalise_website_cell(cell)?;
                vals.insert("website".into(), normalised);
            }
        }
        let new_parents =
            crate::partner_address_sync::parent_names(tx, this.as_records(), &mut vals_list)
                .await?;
        let effective_inputs = vals_list.clone();
        let mut create_context = this.env().context().clone();
        create_context.remove("default_parent_name");
        create_context.set(HIERARCHY_FENCE_CONTEXT_KEY, Value::Bool(has_child_commands));
        let operation = this.with_env(this.env().with_context_replaced(create_context));
        let created: Records = sup.call(tx, operation.as_records(), (vals_list,)).await?;
        let created = created.with_env(this.env().clone());
        guard.admit(&created)?;
        for (record, parent) in created.iter().zip(new_parents) {
            if let Some(parent) = parent {
                let children = record.related(tx, "child_ids").await?;
                if !children.is_empty() {
                    children
                        .write(tx, RawVals::new().set_cell("parent_id", Cell::Id(parent)))
                        .await?;
                }
            }
        }
        let changes =
            crate::partner_address_sync::created_changes(tx, &created, &effective_inputs).await?;
        created
            .call::<_, ()>(tx, "_fields_sync", (changes,))
            .await?;
        Ok(created)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        // the user-type flag is the escape of the company restriction: only the
        // server's compute and the group recompute set it, never a caller
        if vals.contains("partner_share") && !this.env().is_superuser() {
            // a caller without write access to the field gets the access error first
            this.as_records()
                .check_access(tx, erp_sdk::Operation::Write)
                .await?;
            erp_sdk::__private::base_check_write_fields(tx, this.as_records(), &vals)?;
            return Err(Error::programming(
                "'res.partner.partner_share' is set by the server and cannot be written",
            ));
        }
        // Normalise website if updated
        if let Some(cell) = vals.get("website") {
            let normalised = normalise_website_cell(cell)?;
            vals.insert("website".into(), normalised);
        }

        if this.is_empty() || vals.is_empty() {
            return sup.call(tx, this.as_records(), (vals,)).await;
        }
        let _guard = crate::partner_address_sync::OperationGuard::enter(tx, this.as_records())?;
        let before =
            crate::partner_address_sync::before_write(tx, this.as_records(), &vals).await?;
        let structural = vals.contains("parent_id") || carries_child_commands(&vals);
        let operation = this.with_context([(HIERARCHY_FENCE_CONTEXT_KEY, Value::Bool(structural))]);
        sup.call(tx, operation.as_records(), (vals.clone(),))
            .await?;
        let changes =
            crate::partner_address_sync::changed_values(tx, this.as_records(), &vals, &before)
                .await?;
        this.as_records().call(tx, "_fields_sync", (changes,)).await
    }

    /// Address synchronization fields; native addons append through ordinary super dispatch.
    #[method]
    async fn _address_fields(
        _this: &RecordSet<ResPartner>,
        _tx: &mut Transaction,
    ) -> Result<Vec<String>> {
        Ok(
            ["street", "street2", "zip", "city", "state_id", "country_id"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }

    /// Native addons append through ordinary
    /// super dispatch; the base list is `vat` (`additional_identifiers` is a later
    /// base task, commercial amendment).
    #[method]
    async fn _synced_commercial_fields(
        _this: &RecordSet<ResPartner>,
        _tx: &mut Transaction,
    ) -> Result<Vec<String>> {
        Ok(vec!["vat".to_owned()])
    }

    #[method]
    async fn _commercial_fields(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
    ) -> Result<Vec<String>> {
        let mut fields: Vec<String> = this
            .as_records()
            .call(tx, "_synced_commercial_fields", ())
            .await?;
        fields.push("industry_id".to_owned());
        Ok(fields)
    }

    /// Formatting has a separate extension chain from address synchronization.
    #[method]
    async fn _formatting_address_fields(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
    ) -> Result<Vec<String>> {
        this.as_records().call(tx, "_address_fields", ()).await
    }

    /// Propose caller-authorized parent values on the isolated virtual contact.
    #[onchange("parent_id")]
    async fn _onchange_parent_address(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let records = this.as_records();
        let parent = records.related(tx, "parent_id").await?;
        if parent.is_empty() {
            return Ok(OnchangeResult::none());
        }
        let mut proposed = RawVals::new().set_cell(
            "company_id",
            parent
                .cells(tx, "company_id")
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    Error::programming("Partner onchange requires one parent company value")
                })?,
        );
        if records.value(tx, "type").await? == Value::str("contact") {
            let names = crate::partner_address::fields(tx, &parent, "_address_fields").await?;
            let address = crate::partner_address::cells(tx, &parent, &names)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    Error::programming("Partner onchange requires one parent address")
                })?;
            if crate::partner_address::has_address(&address) {
                for (name, value) in address.iter() {
                    proposed.insert(name.to_owned().into(), value.clone());
                }
            }
        }
        let proposed = crate::partner_address::replacement(records, &proposed)?;
        records.write_base(tx, proposed).await?;
        Ok(OnchangeResult::none())
    }

    /// Ordered native synchronization. One effective change map per record, in recordset order.
    #[method]
    async fn _fields_sync(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        changes: Vec<RawVals>,
    ) -> Result<()> {
        crate::partner_address_sync::synchronize(tx, this.as_records(), changes).await
    }

    /// Resolve address types through the caller-authorized bounded tree. Native Rust only.
    #[method]
    async fn address_get(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        adr_pref: Vec<String>,
    ) -> Result<ValueMap> {
        crate::partner_address::selection(tx, this.as_records(), adr_pref).await
    }

    #[method(override)]
    async fn unlink(this: &RecordSet<ResPartner>, tx: &mut Transaction, sup: Super) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await
    }
    /// Search provider for nonstored `has_vat`.
    #[search_method]
    fn _search_has_vat(
        req: &FieldSearchRequest<'_>,
        _ctx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        use erp_sdk::domain::Value as DomainValue;

        let is_true = match req.operator() {
            Operator::Eq => match req.operand() {
                DomainValue::Bool(b) => *b,
                other => {
                    return Err(Error::programming(format!(
                        "expected boolean operand for Eq on has_vat, got {other:?}"
                    )));
                }
            },
            Operator::Ne => match req.operand() {
                DomainValue::Bool(b) => !*b,
                other => {
                    return Err(Error::programming(format!(
                        "expected boolean operand for Ne on has_vat, got {other:?}"
                    )));
                }
            },
            Operator::In => match req.operand() {
                DomainValue::List(list) => {
                    let has_t = list.iter().any(|v| matches!(v, DomainValue::Bool(true)));
                    let has_f = list.iter().any(|v| matches!(v, DomainValue::Bool(false)));
                    match (has_t, has_f) {
                        (true, true) => return Ok(Domain::TRUE),
                        (true, false) => true,
                        (false, true) => false,
                        (false, false) => return Ok(Domain::FALSE),
                    }
                }
                DomainValue::Set(set) => {
                    let has_t = set.iter().any(|v| matches!(v, DomainValue::Bool(true)));
                    let has_f = set.iter().any(|v| matches!(v, DomainValue::Bool(false)));
                    match (has_t, has_f) {
                        (true, true) => return Ok(Domain::TRUE),
                        (true, false) => true,
                        (false, true) => false,
                        (false, false) => return Ok(Domain::FALSE),
                    }
                }
                other => {
                    return Err(Error::programming(format!(
                        "expected list or set of booleans for In on has_vat, got {other:?}"
                    )));
                }
            },
            Operator::NotIn => match req.operand() {
                DomainValue::List(list) => {
                    let has_t = list.iter().any(|v| matches!(v, DomainValue::Bool(true)));
                    let has_f = list.iter().any(|v| matches!(v, DomainValue::Bool(false)));
                    match (has_t, has_f) {
                        (true, true) => return Ok(Domain::FALSE),
                        (true, false) => false,
                        (false, true) => true,
                        (false, false) => return Ok(Domain::TRUE),
                    }
                }
                DomainValue::Set(set) => {
                    let has_t = set.iter().any(|v| matches!(v, DomainValue::Bool(true)));
                    let has_f = set.iter().any(|v| matches!(v, DomainValue::Bool(false)));
                    match (has_t, has_f) {
                        (true, true) => return Ok(Domain::FALSE),
                        (true, false) => false,
                        (false, true) => true,
                        (false, false) => return Ok(Domain::TRUE),
                    }
                }
                other => {
                    return Err(Error::programming(format!(
                        "expected list or set of booleans for NotIn on has_vat, got {other:?}"
                    )));
                }
            },
            other => {
                return Err(Error::programming(format!(
                    "unsupported operator '{other:?}' for has_vat search_method"
                )));
            }
        };

        let c_not_null = Domain::condition("vat", Operator::Ne, false)?;
        let c_not_empty = Domain::condition("vat", Operator::Ne, "")?;
        let c_not_slash = Domain::condition("vat", Operator::Ne, "/")?;
        let c_not_na = Domain::condition("vat", Operator::NotIn, vec!["na", "NA"])?;
        let true_domain = c_not_null & c_not_empty & c_not_slash & c_not_na;

        if is_true {
            Ok(true_domain)
        } else {
            Ok(!true_domain)
        }
    }
}
