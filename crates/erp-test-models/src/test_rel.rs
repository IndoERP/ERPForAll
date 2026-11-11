use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_rel");

// ----- partners, countries, categories ------------------------------------------------------

/// Like `test_orm.partner`.
#[model(
    name = "test_rel.partner",
    description = "Rel Partner",
    addon = "test_rel"
)]
pub struct Partner {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub email: Option<Arc<str>>,
    #[field(char)]
    pub website: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(many2one, comodel = "test_rel.country")]
    pub country_id: Option<RecId>,
    #[field(many2one, comodel = "test_rel.partner")]
    pub parent_id: Option<RecId>,
    #[field(one2many, comodel = "test_rel.partner", inverse_name = "parent_id")]
    pub child_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_rel.category",
        column1 = "partner_id",
        column2 = "category_id",
        relation = "test_rel_category_partner_rel"
    )]
    pub category_id: Vec<RecId>,
    #[field(many2one, comodel = "test_rel.business")]
    pub business_id: Option<RecId>,
    #[field(char, compute = "_compute_vat")]
    pub vat: Option<Arc<str>>,
}

#[methods(Partner, addon = "test_rel")]
impl Partner {
    #[compute(depends = ["name"])]
    fn _compute_vat(batch: &mut Batch<'_, Partner>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Partner::NAME)?;
            batch.set(i, Partner::VAT, name)?;
        }
        Ok(())
    }
}

/// Like `test_orm.country`.
#[model(
    name = "test_rel.country",
    description = "Rel Country",
    addon = "test_rel"
)]
pub struct Country {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub code: Option<Arc<str>>,
}

/// Like `test_orm.partner.category` (hierarchical, mirror of the partners' categories).
#[model(
    name = "test_rel.category",
    description = "Rel Category",
    addon = "test_rel"
)]
pub struct Category {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(many2one, comodel = "test_rel.category", ondelete = "cascade")]
    pub parent_id: Option<RecId>,
    #[field(one2many, comodel = "test_rel.category", inverse_name = "parent_id")]
    pub child_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_rel.partner",
        column1 = "category_id",
        column2 = "partner_id",
        relation = "test_rel_category_partner_rel"
    )]
    pub partner_ids: Vec<RecId>,
}

/// Like `test_orm.business` (a partner's business; nullable many2one from partners).
#[model(
    name = "test_rel.business",
    description = "Rel Business",
    addon = "test_rel"
)]
pub struct Business {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(many2one, comodel = "test_rel.partner", required)]
    pub partner_id: Option<RecId>,
}

// ----- F-ORDER --------------------------------------------------------------------------------

/// `test_performance.base`.
#[model(name = "test_rel.base", description = "Rel Base", addon = "test_rel")]
pub struct BaseOrder {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer, default = 0)]
    pub value: i32,
    #[field(float)]
    pub value_pc: f64,
    #[field(many2one, comodel = "test_rel.partner", string = "Customer")]
    pub partner_id: Option<RecId>,
    #[field(one2many, comodel = "test_rel.line", inverse_name = "base_id")]
    pub line_ids: Vec<RecId>,
    #[field(many2many, comodel = "test_rel.tag")]
    pub tag_ids: Vec<RecId>,
}

/// `test_performance.line`.
#[model(
    name = "test_rel.line",
    description = "Rel Line",
    addon = "test_rel",
    unique_indexes = [("_base_value_unique", "(base_id, value)", "One value per base")]
)]
pub struct Line {
    #[field(many2one, comodel = "test_rel.base", required, ondelete = "cascade")]
    pub base_id: Option<RecId>,
    #[field(integer)]
    pub value: i32,
}

#[methods(Line, addon = "test_rel")]
impl Line {
    #[method(override)]
    async fn unlink(this: &RecordSet<Line>, tx: &mut Transaction, sup: Super) -> Result<()> {
        let values = this.mapped(tx, Line::VALUE).await?;
        if !values.contains(&1001) {
            return sup.call(tx, this.as_records(), ()).await;
        }
        let bases = this.as_records().related(tx, "base_id").await?;
        let rewards = this
            .search(
                tx,
                &(Domain::field("base_id")?.in_(bases.ids()) & Domain::field("value")?.eq(1002)),
            )
            .await?;
        let all = this.union(&rewards)?;
        sup.call(tx, all.as_records(), ()).await
    }

    /// A line created without a value takes its order's value: it shows that the order's own
    /// values are written before its lines.
    #[method(override)]
    async fn create(
        this: &RecordSet<Line>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let bases = tx.env().records("test_rel.base")?;
        for vals in &mut vals_list {
            if vals.contains("value") {
                continue;
            }
            if let Some(Cell::Id(base)) = vals.get("base_id").cloned() {
                let value = bases.browse([base])?.value(tx, "value").await?;
                if let Value::Int(v) = value
                    && let Ok(v) = i32::try_from(v)
                {
                    vals.insert("value".into(), Cell::Int(v));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }
}

/// `test_performance.tag`.
#[model(name = "test_rel.tag", description = "Rel Tag", addon = "test_rel")]
pub struct Tag {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

// ----- F-DISC ---------------------------------------------------------------------------------

/// `test_orm.discussion`.
#[model(
    name = "test_rel.discussion",
    description = "Discussion",
    addon = "test_rel"
)]
pub struct Discussion {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(one2many, comodel = "test_rel.message", inverse_name = "discussion")]
    pub messages: Vec<RecId>,
    #[field(
        one2many,
        comodel = "test_rel.message",
        inverse_name = "discussion",
        domain = [("important", "=", true)]
    )]
    pub important_messages: Vec<RecId>,
    #[field(
        one2many,
        comodel = "test_rel.message",
        inverse_name = "discussion",
        domain = [("important", "=", true)]
    )]
    pub very_important_messages: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_rel.dcategory",
        relation = "test_rel_discussion_category",
        column1 = "discussion",
        column2 = "category"
    )]
    pub categories: Vec<RecId>,
}

/// `test_orm.message`.
#[model(name = "test_rel.message", description = "Message", addon = "test_rel")]
pub struct Message {
    #[field(many2one, comodel = "test_rel.discussion", ondelete = "cascade")]
    pub discussion: Option<RecId>,
    #[field(text)]
    pub body: Option<Arc<str>>,
    #[field(boolean)]
    pub important: bool,
    #[field(many2one, comodel = "res.users")]
    pub author: Option<RecId>,
}

/// `test_orm.category` (the discussions' categories, with the mirror many2many).
#[model(
    name = "test_rel.dcategory",
    description = "Discussion Category",
    addon = "test_rel"
)]
pub struct DCategory {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_rel.discussion",
        relation = "test_rel_discussion_category",
        column1 = "category",
        column2 = "discussion"
    )]
    pub discussions: Vec<RecId>,
}

/// A record referencing a discussion through an optional many2one (`set null`).
#[model(
    name = "test_rel.dref",
    description = "Discussion Reference",
    addon = "test_rel"
)]
pub struct DRef {
    #[field(many2one, comodel = "test_rel.discussion")]
    pub discussion_id: Option<RecId>,
}

/// A record referencing a discussion through a required many2one (`restrict`).
#[model(
    name = "test_rel.dreq",
    description = "Discussion Requirement",
    addon = "test_rel"
)]
pub struct DReq {
    #[field(many2one, comodel = "test_rel.discussion", required)]
    pub discussion_id: Option<RecId>,
}

// ----- F-ACTIVE -------------------------------------------------------------------------------

/// `test_orm.model_active_field`.
#[model(
    name = "test_rel.active",
    description = "Active Field",
    addon = "test_rel"
)]
pub struct Active {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(many2one, comodel = "test_rel.active")]
    pub parent_id: Option<RecId>,
    #[field(one2many, comodel = "test_rel.active", inverse_name = "parent_id")]
    pub children_ids: Vec<RecId>,
    #[field(
        one2many,
        comodel = "test_rel.active",
        inverse_name = "parent_id",
        context = [("active_test", false)]
    )]
    pub all_children_ids: Vec<RecId>,
    #[field(
        one2many,
        comodel = "test_rel.active",
        inverse_name = "parent_id",
        context = [("active_test", true)]
    )]
    pub active_children_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_rel.active",
        relation = "test_rel_active_relatives_rel",
        column1 = "source_id",
        column2 = "dest_id"
    )]
    pub relatives_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_rel.active",
        relation = "test_rel_active_relatives_rel",
        column1 = "source_id",
        column2 = "dest_id",
        context = [("active_test", false)]
    )]
    pub all_relatives_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_rel.active",
        relation = "test_rel_active_relatives_rel",
        column1 = "source_id",
        column2 = "dest_id",
        domain = [("name", "=ilike", "a%")]
    )]
    pub a_relatives_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_rel.active",
        relation = "test_rel_active_relatives_rel",
        column1 = "source_id",
        column2 = "dest_id",
        domain = [("name", "=ilike", "z%")]
    )]
    pub z_relatives_ids: Vec<RecId>,
}

// ----- F-MIXED --------------------------------------------------------------------------------

/// The relational part of `test_orm.mixed`.
#[model(name = "test_rel.mixed", description = "Rel Mixed", addon = "test_rel")]
pub struct Mixed {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(reference, selection = [("test_rel.partner", "Partner"), ("res.users", "User")])]
    pub reference: Option<Arc<str>>,
    #[field(char)]
    pub res_model: Option<Arc<str>>,
    #[field(many2one_reference, model_field = "res_model")]
    pub res_id: i64,
    #[field(many2one, comodel = "test_rel.mixed_rel")]
    pub many2one_id: Option<RecId>,
    #[field(one2many, comodel = "test_rel.mixed_rel", inverse_name = "mixed_id")]
    pub one2many_ids: Vec<RecId>,
    #[field(many2many, comodel = "test_rel.mixed_rel")]
    pub many2many_ids: Vec<RecId>,
}

/// `test_orm.mixed_relations`.
#[model(
    name = "test_rel.mixed_rel",
    description = "Rel Mixed Relations",
    addon = "test_rel"
)]
pub struct MixedRel {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.mixed")]
    pub mixed_id: Option<RecId>,
}

/// `test_orm.schema` (relational columns,/023/033).
#[model(
    name = "test_rel.schema",
    description = "Rel Schema",
    addon = "test_rel"
)]
pub struct Schema {
    #[field(many2one, comodel = "test_rel.mixed_rel")]
    pub many2one_id: Option<RecId>,
    #[field(one2many, comodel = "test_rel.schema_line", inverse_name = "schema_id")]
    pub one2many_ids: Vec<RecId>,
    #[field(many2many, comodel = "test_rel.mixed_rel")]
    pub many2many_ids: Vec<RecId>,
}

/// Lines of `test_rel.schema`.
#[model(
    name = "test_rel.schema_line",
    description = "Rel Schema Line",
    addon = "test_rel"
)]
pub struct SchemaLine {
    #[field(many2one, comodel = "test_rel.schema")]
    pub schema_id: Option<RecId>,
}

// ----- F-COMPANY ------------------------------------------------------------------------------

/// `res.company` (fixture subset). A company is its own company (CRUD-061 form a): the declared
/// field below is checked against it. Declared on purpose: a company's fields are not checked by
/// default.
#[model(
    name = "res.company",
    description = "Companies",
    check_company_auto = true,
    addon = "test_rel"
)]
pub struct Company {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub parent_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "test_rel.model_parent",
        string = "Parent Record",
        check_company
    )]
    pub model_parent_id: Option<RecId>,
}

/// `test_orm.model_parent`.
#[model(
    name = "test_rel.model_parent",
    company_access = unrestricted(reason = "relational company-consistency fixture; access is not under test"),
    description = "Model Multicompany parent",
    addon = "test_rel"
)]
pub struct ModelParent {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
}

/// `test_orm.model_child`.
#[model(
    name = "test_rel.model_child",
    company_access = unrestricted(reason = "relational company-consistency fixture; access is not under test"),
    description = "Model Multicompany child",
    check_company_auto = true,
    addon = "test_rel"
)]
pub struct ModelChild {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "test_rel.model_parent",
        string = "Parent",
        check_company
    )]
    pub parent_id: Option<RecId>,
    #[field(
        many2many,
        comodel = "test_rel.model_parent",
        string = "Parents",
        check_company
    )]
    pub parent_ids: Vec<RecId>,
    /// Hides archived parents when read (field context): the company check still sees them.
    #[field(
        many2many,
        comodel = "test_rel.model_parent",
        relation = "test_rel_model_child_active_parent_rel",
        column1 = "child_id",
        column2 = "parent_id",
        context = [("active_test", true)],
        check_company
    )]
    pub active_parent_ids: Vec<RecId>,
}

/// `test_orm.model_child_nocheck`.
#[model(
    name = "test_rel.model_child_nocheck",
    company_access = unrestricted(reason = "relational company-consistency fixture; access is not under test"),
    description = "Model Multicompany child",
    check_company_auto = true,
    addon = "test_rel"
)]
pub struct ModelChildNocheck {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "res.company")]
    pub company_id: Option<RecId>,
    #[field(many2one, comodel = "test_rel.model_parent", check_company = false)]
    pub parent_id: Option<RecId>,
}

// ----- _inherits ------------------------------------------------------------------------------

/// `test.unit`.
#[model(name = "test_rel.unit", description = "Test Unit", addon = "test_rel")]
pub struct Unit {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(selection, selection = [("a", "A"), ("b", "B")], default = "a")]
    pub state: Option<Arc<str>>,
    #[field(integer)]
    pub size: i32,
    #[field(char, readonly)]
    pub readonly_name: Option<Arc<str>>,
    #[field(one2many, comodel = "test_rel.unit_line", inverse_name = "unit_id")]
    pub line_ids: Vec<RecId>,
}

/// `test.unit.line`.
#[model(
    name = "test_rel.unit_line",
    description = "Test Unit Line",
    addon = "test_rel"
)]
pub struct UnitLine {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.unit", required, ondelete = "cascade")]
    pub unit_id: Option<RecId>,
}

/// `test.box`: delegates to a unit.
#[model(
    name = "test_rel.box",
    description = "Test Box",
    inherits = [("test_rel.unit", "unit_id")],
    addon = "test_rel"
)]
pub struct BoxModel {
    #[field(many2one, comodel = "test_rel.unit", required, ondelete = "cascade")]
    pub unit_id: Option<RecId>,
    #[field(integer)]
    pub size: i32,
    #[field(char)]
    pub field_in_box: Option<Arc<str>>,
    #[field(one2many, comodel = "test_rel.box_note", inverse_name = "box_id")]
    pub note_ids: Vec<RecId>,
}

/// `test.pallet`: delegates to a box (and through it to a unit).
#[model(
    name = "test_rel.pallet",
    description = "Test Pallet",
    inherits = [("test_rel.box", "box_id")],
    addon = "test_rel"
)]
pub struct Pallet {
    #[field(many2one, comodel = "test_rel.box", required, ondelete = "cascade")]
    pub box_id: Option<RecId>,
    #[field(char)]
    pub field_in_pallet: Option<Arc<str>>,
}

/// `test.another_unit`.
#[model(
    name = "test_rel.another_unit",
    description = "Another Test Unit",
    addon = "test_rel"
)]
pub struct AnotherUnit {
    #[field(integer, required)]
    pub val1: i32,
}

/// `test.another_box`: its constraint reads a delegated field.
#[model(
    name = "test_rel.another_box",
    description = "Another Test Box",
    inherits = [("test_rel.another_unit", "another_unit_id")],
    addon = "test_rel"
)]
pub struct AnotherBox {
    #[field(
        many2one,
        comodel = "test_rel.another_unit",
        required,
        ondelete = "cascade"
    )]
    pub another_unit_id: Option<RecId>,
    #[field(integer, required)]
    pub val2: i32,
}

#[methods(AnotherBox, addon = "test_rel")]
impl AnotherBox {
    #[constrains("val1", "val2")]
    async fn _check_values(this: &RecordSet<AnotherBox>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        let val1 = records.values(tx, "val1").await?;
        let val2 = records.values(tx, "val2").await?;
        if val1.iter().zip(&val2).any(|(a, b)| a != b) {
            return Err(Error::validation("The two values must be equals"));
        }
        Ok(())
    }
}

// ----- joins (test_orm.search.*) and test_orm.multi ---------------------------------------------

/// `test_orm.search.top_rel`.
#[model(name = "test_rel.top", description = "Top", addon = "test_rel")]
pub struct Top {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.mid", required)]
    pub mid_rel_req_id: Option<RecId>,
    #[field(many2one, comodel = "test_rel.mid")]
    pub mid_rel_id: Option<RecId>,
}

/// `test_orm.search.mid_rel`.
#[model(name = "test_rel.mid", description = "Mid", addon = "test_rel")]
pub struct Mid {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.bot", required)]
    pub bot_rel_req_id: Option<RecId>,
    #[field(many2one, comodel = "test_rel.bot")]
    pub bot_rel_id: Option<RecId>,
}

/// `test_orm.search.bot_rel`.
#[model(name = "test_rel.bot", description = "Bot", addon = "test_rel")]
pub struct Bot {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `test_orm.multi`.
#[model(name = "test_rel.multi", description = "Multi", addon = "test_rel")]
pub struct Multi {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.partner")]
    pub partner: Option<RecId>,
    #[field(one2many, comodel = "test_rel.multi_line", inverse_name = "multi")]
    pub lines: Vec<RecId>,
    #[field(many2many, comodel = "test_rel.multi_tag", domain = [("name", "ilike", "a")])]
    pub tags: Vec<RecId>,
}

/// `test_orm.multi.line`.
#[model(
    name = "test_rel.multi_line",
    description = "Multi Line",
    addon = "test_rel"
)]
pub struct MultiLine {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.multi", ondelete = "cascade")]
    pub multi: Option<RecId>,
    #[field(many2many, comodel = "test_rel.multi_tag")]
    pub tags: Vec<RecId>,
}

/// `test_orm.multi.tag`.
#[model(
    name = "test_rel.multi_tag",
    description = "Multi Tag",
    addon = "test_rel"
)]
pub struct MultiTag {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

// ----- ordering by many2one ---------------------------------------------------------------------

/// A country ordered by its phone code.
#[model(
    name = "test_rel.ocountry",
    description = "Ordered Country",
    order = "phone_code",
    addon = "test_rel"
)]
pub struct OCountry {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub phone_code: Option<Arc<str>>,
}

/// Partners ordered through their country.
#[model(
    name = "test_rel.opartner",
    description = "Ordered Partner",
    addon = "test_rel"
)]
pub struct OPartner {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.ocountry")]
    pub country_id: Option<RecId>,
}

/// `test_orm.alpha`-like self-referencing order.
#[model(
    name = "test_rel.alpha",
    description = "Alpha",
    order = "alpha_loop_id desc, name",
    addon = "test_rel"
)]
pub struct Alpha {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.alpha")]
    pub alpha_loop_id: Option<RecId>,
}

// ----- many2many ondelete (test_orm.model_a / model_b) -------------------------------------------

/// `test_orm.model_a`.
#[model(name = "test_rel.model_a", description = "Model A", addon = "test_rel")]
pub struct ModelA {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_rel.model_b",
        relation = "rel_model_a_model_b_1",
        column1 = "a_id",
        column2 = "b_id"
    )]
    pub a_restricted_b_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_rel.model_b",
        relation = "rel_model_a_model_b_2",
        column1 = "a_id",
        column2 = "b_id",
        ondelete = "restrict"
    )]
    pub b_restricted_b_ids: Vec<RecId>,
}

/// `test_orm.model_b`.
#[model(name = "test_rel.model_b", description = "Model B", addon = "test_rel")]
pub struct ModelB {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_rel.model_a",
        relation = "rel_model_a_model_b_1",
        column1 = "b_id",
        column2 = "a_id",
        ondelete = "restrict"
    )]
    pub a_restricted_a_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_rel.model_a",
        relation = "rel_model_a_model_b_2",
        column1 = "b_id",
        column2 = "a_id"
    )]
    pub b_restricted_a_ids: Vec<RecId>,
}

// ----- review round ------------------------------------------------------------------

/// A stored compute over a many2one that a deletion sets null.
#[model(name = "test_rel.holder", description = "Holder", addon = "test_rel")]
pub struct Holder {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.country", ondelete = "set null")]
    pub country_id: Option<RecId>,
    #[field(boolean, compute = "_compute_has_country", store)]
    pub has_country: bool,
}

#[methods(Holder, addon = "test_rel")]
impl Holder {
    #[compute(depends = ["country_id"])]
    fn _compute_has_country(batch: &mut Batch<'_, Holder>) -> Result<()> {
        for i in batch.indices() {
            let country = batch.get(i, Holder::COUNTRY_ID)?;
            batch.set(i, Holder::HAS_COUNTRY, country.is_some())?;
        }
        Ok(())
    }
}

#[model(name = "test_rel.counter", description = "Counter", addon = "test_rel")]
pub struct Counter {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        one2many,
        comodel = "test_rel.counter_item",
        inverse_name = "counter_id"
    )]
    pub item_ids: X2many<CounterItem>,
    #[field(integer, compute = "_compute_item_count", store)]
    pub item_count: i32,
}

#[methods(Counter, addon = "test_rel")]
impl Counter {
    #[compute(depends = ["item_ids"])]
    fn _compute_item_count(batch: &mut Batch<'_, Counter>) -> Result<()> {
        for i in batch.indices() {
            let items = batch.get(i, Counter::ITEM_IDS)?;
            batch.set(
                i,
                Counter::ITEM_COUNT,
                i32::try_from(items.len()).unwrap_or(i32::MAX),
            )?;
        }
        Ok(())
    }
}

/// A line of [`Counter`].
#[model(
    name = "test_rel.counter_item",
    description = "Counter Item",
    addon = "test_rel"
)]
pub struct CounterItem {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.counter", ondelete = "set null")]
    pub counter_id: Many2one<Counter>,
}

/// Three levels of foreign keys: `chain_b` cascades from `chain_a`, `chain_c` is set null from
/// `chain_b`.
#[model(name = "test_rel.chain_a", description = "Chain A", addon = "test_rel")]
pub struct ChainA {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// See [`ChainA`].
#[model(name = "test_rel.chain_b", description = "Chain B", addon = "test_rel")]
pub struct ChainB {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.chain_a", ondelete = "cascade")]
    pub a_id: Option<RecId>,
}

/// See [`ChainA`].
#[model(name = "test_rel.chain_c", description = "Chain C", addon = "test_rel")]
pub struct ChainC {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.chain_b", ondelete = "set null")]
    pub b_id: Option<RecId>,
    #[field(one2many, comodel = "test_rel.chain_d", inverse_name = "c_id")]
    pub d_ids: Vec<RecId>,
}

/// Lines of [`ChainC`] that also cascade from `chain_b` (an x2many over an uncached cascade).
#[model(name = "test_rel.chain_d", description = "Chain D", addon = "test_rel")]
pub struct ChainD {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.chain_c")]
    pub c_id: Option<RecId>,
    #[field(many2one, comodel = "test_rel.chain_b", ondelete = "cascade")]
    pub b_id: Option<RecId>,
}

/// Records with attachments: a one2many over a many2one_reference (F2).
#[model(
    name = "test_rel.attach_owner",
    description = "Attachment Owner",
    addon = "test_rel"
)]
pub struct AttachOwner {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(one2many, comodel = "test_rel.attachment", inverse_name = "res_id")]
    pub attachment_ids: Vec<RecId>,
}

/// Like `ir.attachment`: `res_model` + `res_id`.
#[model(
    name = "test_rel.attachment",
    description = "Attachment",
    addon = "test_rel"
)]
pub struct Attachment {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub res_model: Option<Arc<str>>,
    #[field(many2one_reference, model_field = "res_model")]
    pub res_id: i64,
}

/// Notes of a box: an own one2many on a model with `_inherits`.
#[model(
    name = "test_rel.box_note",
    description = "Box Note",
    addon = "test_rel"
)]
pub struct BoxNote {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rel.box", ondelete = "cascade")]
    pub box_id: Option<RecId>,
}
