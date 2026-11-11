use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_depends");

// ----- partners, orders, lines (BENCH-005 shape) -------------------------------------------------

/// A country.
#[model(
    name = "test_dep.country",
    description = "Dep Country",
    addon = "test_depends"
)]
pub struct Country {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub code: Option<Arc<str>>,
}

/// A customer with the total of its orders (two relational levels of stored computes).
#[model(
    name = "test_dep.partner",
    description = "Dep Partner",
    addon = "test_depends"
)]
pub struct Partner {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_dep.country")]
    pub country_id: Many2one<Country>,
    #[field(one2many, comodel = "test_dep.order", inverse_name = "partner_id")]
    pub order_ids: X2many<Order>,
    #[field(float, compute = "_compute_total_amount", store)]
    pub total_amount: f64,
    #[field(char, related = "country_id.code")]
    pub country_code: Option<Arc<str>>,
}

#[methods(Partner, addon = "test_depends")]
impl Partner {
    #[compute(depends = ["order_ids.amount_total"])]
    fn _compute_total_amount(batch: &mut Batch<'_, Partner>) -> Result<()> {
        for i in batch.indices() {
            let orders = batch.get(i, Partner::ORDER_IDS)?;
            let total: f64 = batch
                .values_of(Order::AMOUNT_TOTAL, orders.ids())?
                .iter()
                .sum();
            batch.set(i, Partner::TOTAL_AMOUNT, total)?;
        }
        Ok(())
    }
}

/// `bench.order`: `amount_total = sum(line_ids.price_subtotal)` stored.
#[model(
    name = "test_dep.order",
    description = "Dep Order",
    addon = "test_depends"
)]
pub struct Order {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_dep.partner")]
    pub partner_id: Many2one<Partner>,
    #[field(char, related = "partner_id.name")]
    pub partner_name: Option<Arc<str>>,
    #[field(char, related = "partner_id.name", readonly = false)]
    pub partner_name_rw: Option<Arc<str>>,
    #[field(char, related = "partner_id.country_id.code", store)]
    pub partner_country_code: Option<Arc<str>>,
    #[field(one2many, comodel = "test_dep.line", inverse_name = "order_id")]
    pub line_ids: X2many<Line>,
    #[field(float, compute = "_compute_amount_total", store)]
    pub amount_total: f64,
    #[field(integer, compute = "_compute_line_count")]
    pub line_count: i32,
    /// An inverse one2many over a non-stored related many2one, with a domain.
    #[field(
        one2many,
        comodel = "test_dep.line",
        inverse_name = "related_order_id",
        domain = [("price_unit", ">=", 0)]
    )]
    pub related_line_ids: X2many<Line>,
}

#[methods(Order, addon = "test_depends")]
impl Order {
    #[compute(depends = ["line_ids.price_subtotal"])]
    fn _compute_amount_total(batch: &mut Batch<'_, Order>) -> Result<()> {
        for i in batch.indices() {
            let lines = batch.get(i, Order::LINE_IDS)?;
            let total: f64 = batch
                .values_of(Line::PRICE_SUBTOTAL, lines.ids())?
                .iter()
                .sum();
            batch.set(i, Order::AMOUNT_TOTAL, total)?;
        }
        Ok(())
    }

    #[compute(depends = ["line_ids"])]
    fn _compute_line_count(batch: &mut Batch<'_, Order>) -> Result<()> {
        for i in batch.indices() {
            let lines = batch.get(i, Order::LINE_IDS)?;
            batch.set(
                i,
                Order::LINE_COUNT,
                i32::try_from(lines.len()).unwrap_or(i32::MAX),
            )?;
        }
        Ok(())
    }
}

/// `bench.order.line`: `price_subtotal = qty * price_unit` stored, plus a stored related field
/// two many2one hops away (precomputed at create). Fill factor 80: recomputes rewrite many lines
/// at once, so pages keep room for HOT updates.
#[model(
    name = "test_dep.line",
    description = "Dep Line",
    addon = "test_depends",
    fillfactor = 80
)]
pub struct Line {
    #[field(many2one, comodel = "test_dep.order", required, ondelete = "cascade")]
    pub order_id: Many2one<Order>,
    #[field(char)]
    pub product: Option<Arc<str>>,
    #[field(float, default = 1.0)]
    pub qty: f64,
    #[field(float)]
    pub price_unit: f64,
    #[field(float, compute = "_compute_price_subtotal", store)]
    pub price_subtotal: f64,
    #[field(char, related = "order_id.partner_id.name", store)]
    pub order_partner: Option<Arc<str>>,
    /// `test_performance.line.related_base_id`.
    #[field(many2one, comodel = "test_dep.order", related = "order_id")]
    pub related_order_id: Many2one<Order>,
    /// The chain tags of this line ([`ChainTag`], a second stored related level).
    #[field(one2many, comodel = "test_dep.chain_tag", inverse_name = "line_id")]
    pub chain_tag_ids: X2many<ChainTag>,
}

#[methods(Line, addon = "test_depends")]
impl Line {
    #[compute(depends = ["qty", "price_unit"])]
    fn _compute_price_subtotal(batch: &mut Batch<'_, Line>) -> Result<()> {
        for i in batch.indices() {
            let qty = batch.get(i, Line::QTY)?;
            let price = batch.get(i, Line::PRICE_UNIT)?;
            batch.set(i, Line::PRICE_SUBTOTAL, qty * price)?;
        }
        Ok(())
    }
}

/// A second stored related level (tags of a line): `line_partner` is stored from `line_id.order_partner`, itself a
/// stored related field, so a rename of the partner reaches it only when the line recomputes.
#[model(
    name = "test_dep.chain_tag",
    description = "Dep Chain Tag",
    addon = "test_depends"
)]
pub struct ChainTag {
    #[field(many2one, comodel = "test_dep.line", required, ondelete = "cascade")]
    pub line_id: Many2one<Line>,
    #[field(char, related = "line_id.order_partner", store)]
    pub line_partner: Option<Arc<str>>,
}

/// `test_orm.precompute.required`: a required stored related field, precomputed.
#[model(
    name = "test_dep.required",
    description = "Dep Required",
    addon = "test_depends"
)]
pub struct Required {
    #[field(many2one, comodel = "test_dep.partner", required)]
    pub partner_id: Many2one<Partner>,
    #[field(char, related = "partner_id.name", store, required)]
    pub name: Option<Arc<str>>,
}

// ----- an inverse rewriting a stored compute's input inside create ---------------

/// `x` (non-stored, inversed) writes `y` on its own record after the INSERT; `z` is stored over
/// `y` without `precompute`, so it is precomputed into the INSERT but not protected.
#[model(
    name = "test_dep.inverse_input",
    description = "Dep Inverse Input",
    addon = "test_depends"
)]
pub struct InverseInput {
    #[field(char)]
    pub y: Option<Arc<str>>,
    #[field(char, compute = "_compute_x", inverse = "_inverse_x")]
    pub x: Option<Arc<str>>,
    #[field(char, compute = "_compute_z", store)]
    pub z: Option<Arc<str>>,
}

#[methods(InverseInput, addon = "test_depends")]
impl InverseInput {
    #[compute(depends = ["y"])]
    fn _compute_x(batch: &mut Batch<'_, InverseInput>) -> Result<()> {
        for i in batch.indices() {
            let y = batch.get(i, InverseInput::Y)?;
            batch.set(
                i,
                InverseInput::X,
                y.map(|v| Arc::<str>::from(v.to_lowercase())),
            )?;
        }
        Ok(())
    }

    /// `y` becomes `x` in upper case.
    #[method]
    async fn _inverse_x(this: &RecordSet<InverseInput>, tx: &mut Transaction) -> Result<()> {
        for rec in this.iter() {
            let x = rec.get(tx, InverseInput::X).await?;
            rec.set(
                tx,
                InverseInput::Y,
                x.map(|v| Arc::<str>::from(v.to_uppercase())),
            )
            .await?;
        }
        Ok(())
    }

    #[compute(depends = ["y"])]
    fn _compute_z(batch: &mut Batch<'_, InverseInput>) -> Result<()> {
        for i in batch.indices() {
            let y = batch.get(i, InverseInput::Y)?;
            batch.set(
                i,
                InverseInput::Z,
                format!("z:{}", y.as_deref().unwrap_or("")).as_str(),
            )?;
        }
        Ok(())
    }
}

// ----- discussions and messages --------------------------------------------

/// `test_orm.discussion` (subset).
#[model(
    name = "test_dep.discussion",
    description = "Dep Discussion",
    addon = "test_depends"
)]
pub struct Discussion {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(one2many, comodel = "test_dep.message", inverse_name = "discussion_id")]
    pub message_ids: X2many<Message>,
    #[field(integer, compute = "_compute_message_count", store)]
    pub message_count: i32,
    /// No inverse field on users.
    #[field(
        many2many,
        comodel = "test_dep.user",
        relation = "test_dep_discussion_participant_rel",
        column1 = "discussion_id",
        column2 = "user_id"
    )]
    pub participants: X2many<User>,
}

#[methods(Discussion, addon = "test_depends")]
impl Discussion {
    #[compute(depends = ["message_ids"])]
    fn _compute_message_count(batch: &mut Batch<'_, Discussion>) -> Result<()> {
        for i in batch.indices() {
            let messages = batch.get(i, Discussion::MESSAGE_IDS)?;
            batch.set(
                i,
                Discussion::MESSAGE_COUNT,
                i32::try_from(messages.len()).unwrap_or(i32::MAX),
            )?;
        }
        Ok(())
    }
}

/// `test_orm.message` (subset): stored `name = "[<discussion>] <author>"`, editable related
/// `discussion_name`.
#[model(
    name = "test_dep.message",
    description = "Dep Message",
    addon = "test_depends"
)]
pub struct Message {
    #[field(many2one, comodel = "test_dep.discussion", ondelete = "cascade")]
    pub discussion_id: Many2one<Discussion>,
    #[field(char)]
    pub author: Option<Arc<str>>,
    #[field(char, compute = "_compute_name", store)]
    pub name: Option<Arc<str>>,
    #[field(char, related = "discussion_id.name", readonly = false)]
    pub discussion_name: Option<Arc<str>>,
    #[field(text)]
    pub body: Option<Arc<str>>,
}

#[methods(Message, addon = "test_depends")]
impl Message {
    #[compute(depends = ["author", "discussion_id.name"])]
    fn _compute_name(batch: &mut Batch<'_, Message>) -> Result<()> {
        for i in batch.indices() {
            let discussion = batch.get(i, Message::DISCUSSION_ID)?;
            let name = match discussion.id() {
                Some(d) => batch.value_of(Discussion::NAME, d)?,
                None => None,
            };
            let author = batch.get(i, Message::AUTHOR)?;
            batch.set(
                i,
                Message::NAME,
                format!(
                    "[{}] {}",
                    name.as_deref().unwrap_or(""),
                    author.as_deref().unwrap_or("")
                )
                .as_str(),
            )?;
        }
        Ok(())
    }
}

// ----- test_orm.modified* --------------------------------------------

/// `test_orm.modified`.
#[model(
    name = "test_dep.modified",
    description = "Dep Modified",
    addon = "test_depends"
)]
pub struct Modified {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        one2many,
        comodel = "test_dep.modified_line",
        inverse_name = "modified_id"
    )]
    pub line_ids: X2many<ModifiedLine>,
    #[field(integer, compute = "_compute_total_quantity")]
    pub total_quantity: i32,
}

#[methods(Modified, addon = "test_depends")]
impl Modified {
    #[compute(depends = ["line_ids.quantity"])]
    fn _compute_total_quantity(batch: &mut Batch<'_, Modified>) -> Result<()> {
        for i in batch.indices() {
            let lines = batch.get(i, Modified::LINE_IDS)?;
            let total: i32 = batch
                .values_of(ModifiedLine::QUANTITY, lines.ids())?
                .iter()
                .sum();
            batch.set(i, Modified::TOTAL_QUANTITY, total)?;
        }
        Ok(())
    }
}

/// `test_orm.modified.line`: a recursive non-stored `total_price` over the children.
#[model(
    name = "test_dep.modified_line",
    description = "Dep Modified Line",
    addon = "test_depends"
)]
pub struct ModifiedLine {
    #[field(many2one, comodel = "test_dep.modified")]
    pub modified_id: Many2one<Modified>,
    #[field(char, related = "modified_id.name")]
    pub modified_name: Option<Arc<str>>,
    #[field(integer)]
    pub quantity: i32,
    #[field(float)]
    pub price: f64,
    #[field(float, compute = "_compute_total_price")]
    pub total_price: f64,
    #[field(float, compute = "_compute_total_price_quantity")]
    pub total_price_quantity: f64,
    #[field(many2one, comodel = "test_dep.modified_line")]
    pub parent_id: Many2one<ModifiedLine>,
    #[field(
        one2many,
        comodel = "test_dep.modified_line",
        inverse_name = "parent_id"
    )]
    pub child_ids: X2many<ModifiedLine>,
}

#[methods(ModifiedLine, addon = "test_depends")]
impl ModifiedLine {
    #[compute(depends = ["price", "child_ids.total_price", "child_ids.price"])]
    fn _compute_total_price(batch: &mut Batch<'_, ModifiedLine>) -> Result<()> {
        for i in batch.indices() {
            let children = batch.get(i, ModifiedLine::CHILD_IDS)?;
            let below: f64 = batch
                .values_of(ModifiedLine::TOTAL_PRICE, children.ids())?
                .iter()
                .sum();
            let price = batch.get(i, ModifiedLine::PRICE)?;
            batch.set(i, ModifiedLine::TOTAL_PRICE, below + price)?;
        }
        Ok(())
    }

    #[compute(depends = ["total_price", "quantity"])]
    fn _compute_total_price_quantity(batch: &mut Batch<'_, ModifiedLine>) -> Result<()> {
        for i in batch.indices() {
            let total = batch.get(i, ModifiedLine::TOTAL_PRICE)?;
            let quantity = batch.get(i, ModifiedLine::QUANTITY)?;
            batch.set(
                i,
                ModifiedLine::TOTAL_PRICE_QUANTITY,
                total * f64::from(quantity),
            )?;
        }
        Ok(())
    }
}

/// `test_orm.modified.line.positive`: a many2many without inverse, and a non-stored compute
/// through it.
#[model(
    name = "test_dep.modified_positive",
    description = "Dep Modified Positive",
    addon = "test_depends"
)]
pub struct ModifiedPositive {
    #[field(many2many, comodel = "test_dep.modified_line")]
    pub line_ids: X2many<ModifiedLine>,
    #[field(boolean, compute = "_compute_is_positive")]
    pub is_positive: bool,
}

#[methods(ModifiedPositive, addon = "test_depends")]
impl ModifiedPositive {
    #[compute(depends = ["line_ids.quantity"])]
    fn _compute_is_positive(batch: &mut Batch<'_, ModifiedPositive>) -> Result<()> {
        for i in batch.indices() {
            let lines = batch.get(i, ModifiedPositive::LINE_IDS)?;
            let total: i32 = batch
                .values_of(ModifiedLine::QUANTITY, lines.ids())?
                .iter()
                .sum();
            batch.set(i, ModifiedPositive::IS_POSITIVE, total > 0)?;
        }
        Ok(())
    }
}

// ----- test_orm.recursive ---------------------------------------------------

#[model(
    name = "test_dep.recursive",
    description = "Dep Recursive",
    addon = "test_depends"
)]
pub struct Recursive {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_dep.recursive", ondelete = "cascade")]
    pub parent: Many2one<Recursive>,
    #[field(char, compute = "_compute_full_name")]
    pub full_name: Option<Arc<str>>,
    #[field(char, compute = "_compute_display_label", store)]
    pub display_label: Option<Arc<str>>,
}

#[methods(Recursive, addon = "test_depends")]
impl Recursive {
    #[compute(depends = ["name", "parent.full_name"])]
    fn _compute_full_name(batch: &mut Batch<'_, Recursive>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Recursive::NAME)?;
            let value = match batch.get(i, Recursive::PARENT)?.id() {
                Some(p) => format!(
                    "{} / {}",
                    batch
                        .value_of(Recursive::FULL_NAME, p)?
                        .as_deref()
                        .unwrap_or(""),
                    name.as_deref().unwrap_or("")
                ),
                None => name.as_deref().unwrap_or("").to_owned(),
            };
            batch.set(i, Recursive::FULL_NAME, value.as_str())?;
        }
        Ok(())
    }

    #[compute(depends = ["name", "parent.display_label"])]
    fn _compute_display_label(batch: &mut Batch<'_, Recursive>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Recursive::NAME)?;
            let value = match batch.get(i, Recursive::PARENT)?.id() {
                Some(p) => format!(
                    "{} / {}",
                    batch
                        .value_of(Recursive::DISPLAY_LABEL, p)?
                        .as_deref()
                        .unwrap_or(""),
                    name.as_deref().unwrap_or("")
                ),
                None => name.as_deref().unwrap_or("").to_owned(),
            };
            batch.set(i, Recursive::DISPLAY_LABEL, value.as_str())?;
        }
        Ok(())
    }
}

// ----- containers and members -----------------------------------------------------

/// `test_orm.one2many` container (subset).
#[model(
    name = "test_dep.container",
    description = "Dep Container",
    addon = "test_depends"
)]
pub struct Container {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(one2many, comodel = "test_dep.member", inverse_name = "container_id")]
    pub member_ids: X2many<Member>,
    #[field(integer, compute = "_compute_member_count", store)]
    pub member_count: i32,
}

#[methods(Container, addon = "test_depends")]
impl Container {
    #[compute(depends = ["member_ids"])]
    fn _compute_member_count(batch: &mut Batch<'_, Container>) -> Result<()> {
        for i in batch.indices() {
            let members = batch.get(i, Container::MEMBER_IDS)?;
            batch.set(
                i,
                Container::MEMBER_COUNT,
                i32::try_from(members.len()).unwrap_or(i32::MAX),
            )?;
        }
        Ok(())
    }
}

/// A member whose stored many2one `container_id` is computed from its name: the container with
/// the same name.
#[model(
    name = "test_dep.member",
    description = "Dep Member",
    addon = "test_depends"
)]
pub struct Member {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_dep.container",
        compute = "_compute_container_id",
        store
    )]
    pub container_id: Many2one<Container>,
}

#[methods(Member, addon = "test_depends")]
impl Member {
    #[compute(depends = ["name"])]
    async fn _compute_container_id(
        this: &RecordSet<Member>,
        tx: &mut Transaction,
        out: &mut Assign<'_, Member>,
    ) -> Result<()> {
        let names = this.mapped(tx, Member::NAME).await?;
        let containers = this.env().model::<Container>()?;
        for (i, name) in names.iter().enumerate() {
            let found = match name {
                Some(name) => containers
                    .search(tx, &Domain::field("name")?.eq(name.to_string()))
                    .await?
                    .rec_ids()
                    .first()
                    .copied(),
                None => None,
            };
            out.set(i, Member::CONTAINER_ID, found)?;
        }
        Ok(())
    }
}

// ----- groups and users: many2many with a mirror ----------------------------------

/// `test_orm.user` (subset).
#[model(
    name = "test_dep.user",
    description = "Dep User",
    addon = "test_depends"
)]
pub struct User {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_dep.group",
        relation = "test_dep_user_group_rel",
        column1 = "user_id",
        column2 = "group_id"
    )]
    pub group_ids: X2many<Group>,
    #[field(integer, compute = "_compute_group_count", store)]
    pub group_count: i32,
}

#[methods(User, addon = "test_depends")]
impl User {
    #[compute(depends = ["group_ids"])]
    fn _compute_group_count(batch: &mut Batch<'_, User>) -> Result<()> {
        for i in batch.indices() {
            let groups = batch.get(i, User::GROUP_IDS)?;
            batch.set(
                i,
                User::GROUP_COUNT,
                i32::try_from(groups.len()).unwrap_or(i32::MAX),
            )?;
        }
        Ok(())
    }
}

/// `test_orm.group` (subset).
#[model(
    name = "test_dep.group",
    description = "Dep Group",
    addon = "test_depends"
)]
pub struct Group {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_dep.user",
        relation = "test_dep_user_group_rel",
        column1 = "group_id",
        column2 = "user_id"
    )]
    pub user_ids: X2many<User>,
}

// ----- archived children ---------------------------------------------------------

/// A parent with a stored sum over its children's values.
#[model(
    name = "test_dep.tree",
    description = "Dep Tree",
    addon = "test_depends"
)]
pub struct Tree {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(one2many, comodel = "test_dep.leaf", inverse_name = "tree_id")]
    pub leaf_ids: X2many<Leaf>,
    #[field(integer, compute = "_compute_leaf_sum", store)]
    pub leaf_sum: i32,
}

#[methods(Tree, addon = "test_depends")]
impl Tree {
    #[compute(depends = ["leaf_ids.value"])]
    fn _compute_leaf_sum(batch: &mut Batch<'_, Tree>) -> Result<()> {
        for i in batch.indices() {
            let leaves = batch.get(i, Tree::LEAF_IDS)?;
            let sum: i32 = batch.values_of(Leaf::VALUE, leaves.ids())?.iter().sum();
            batch.set(i, Tree::LEAF_SUM, sum)?;
        }
        Ok(())
    }
}

/// A child that can be archived.
#[model(
    name = "test_dep.leaf",
    description = "Dep Leaf",
    addon = "test_depends"
)]
pub struct Leaf {
    #[field(many2one, comodel = "test_dep.tree", ondelete = "cascade")]
    pub tree_id: Many2one<Tree>,
    #[field(integer)]
    pub value: i32,
    #[field(boolean, default = true)]
    pub active: bool,
}

// ----- a transient model ----------------------------------------------------------

/// A wizard whose stored label depends on a regular model's field.
#[model(
    name = "test_dep.wizard",
    description = "Dep Wizard",
    kind = "transient",
    addon = "test_depends"
)]
pub struct Wizard {
    #[field(many2one, comodel = "test_dep.partner")]
    pub partner_id: Many2one<Partner>,
    #[field(char, compute = "_compute_partner_label", store)]
    pub partner_label: Option<Arc<str>>,
}

#[methods(Wizard, addon = "test_depends")]
impl Wizard {
    #[compute(depends = ["partner_id.name"])]
    fn _compute_partner_label(batch: &mut Batch<'_, Wizard>) -> Result<()> {
        for i in batch.indices() {
            let label = match batch.get(i, Wizard::PARTNER_ID)?.id() {
                Some(p) => batch.value_of(Partner::NAME, p)?,
                None => None,
            };
            batch.set(i, Wizard::PARTNER_LABEL, label)?;
        }
        Ok(())
    }
}

// ----- `_inherits`: a dependency on a delegated field --------------------------------------------

/// A unit.
#[model(
    name = "test_dep.unit",
    description = "Dep Unit",
    addon = "test_depends"
)]
pub struct Unit {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A box delegating to a unit, with a stored copy of the unit's name.
#[model(
    name = "test_dep.boxed",
    description = "Dep Box",
    inherits = [("test_dep.unit", "unit_id")],
    addon = "test_depends"
)]
pub struct Boxed {
    #[field(many2one, comodel = "test_dep.unit", required, ondelete = "cascade")]
    pub unit_id: Many2one<Unit>,
    #[field(char, related = "name", store)]
    pub unit_name: Option<Arc<str>>,
}

// ----- cascades of cascades (left open by S3) ----------------------------------------

/// The root of a cascade chain: deleting it deletes its `cas_b` rows in the database.
#[model(
    name = "test_dep.cas_a",
    description = "Dep Cascade A",
    addon = "test_depends"
)]
pub struct CasA {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// Deleted by cascade with its `cas_a`; its own deletion sets `cas_c.b_id` null and deletes its
/// `cas_d` rows.
#[model(
    name = "test_dep.cas_b",
    description = "Dep Cascade B",
    addon = "test_depends"
)]
pub struct CasB {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_dep.cas_a", ondelete = "cascade")]
    pub a_id: Many2one<CasA>,
}

/// Stored computes over a link a cascade sets null and a one2many a cascade empties.
#[model(
    name = "test_dep.cas_c",
    description = "Dep Cascade C",
    addon = "test_depends"
)]
pub struct CasC {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_dep.cas_b", ondelete = "set null")]
    pub b_id: Many2one<CasB>,
    #[field(boolean, compute = "_compute_has_b", store)]
    pub has_b: bool,
    #[field(one2many, comodel = "test_dep.cas_d", inverse_name = "c_id")]
    pub d_ids: X2many<CasD>,
    #[field(integer, compute = "_compute_d_count", store)]
    pub d_count: i32,
}

#[methods(CasC, addon = "test_depends")]
impl CasC {
    #[compute(depends = ["b_id"])]
    fn _compute_has_b(batch: &mut Batch<'_, CasC>) -> Result<()> {
        for i in batch.indices() {
            let b = batch.get(i, CasC::B_ID)?;
            batch.set(i, CasC::HAS_B, b.is_some())?;
        }
        Ok(())
    }

    #[compute(depends = ["d_ids"])]
    fn _compute_d_count(batch: &mut Batch<'_, CasC>) -> Result<()> {
        for i in batch.indices() {
            let d = batch.get(i, CasC::D_IDS)?;
            batch.set(i, CasC::D_COUNT, i32::try_from(d.len()).unwrap_or(i32::MAX))?;
        }
        Ok(())
    }
}

/// Lines of `cas_c` deleted by cascade with their `cas_b`.
#[model(
    name = "test_dep.cas_d",
    description = "Dep Cascade D",
    addon = "test_depends"
)]
pub struct CasD {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_dep.cas_c")]
    pub c_id: Many2one<CasC>,
    #[field(many2one, comodel = "test_dep.cas_b", ondelete = "cascade")]
    pub b_id: Many2one<CasB>,
}

// ----- new records (…058, -061, -081) --------------

/// `test_orm.move`: lines with a domain, and a stored sum over them.
#[model(
    name = "test_dep.move",
    description = "Dep Move",
    addon = "test_depends"
)]
pub struct Move {
    #[field(
        one2many,
        comodel = "test_dep.move_line",
        inverse_name = "move_id",
        domain = [("visible", "=", true)]
    )]
    pub line_ids: X2many<MoveLine>,
    #[field(integer, compute = "_compute_quantity", store)]
    pub quantity: i32,
}

#[methods(Move, addon = "test_depends")]
impl Move {
    #[compute(depends = ["line_ids.quantity"])]
    fn _compute_quantity(batch: &mut Batch<'_, Move>) -> Result<()> {
        for i in batch.indices() {
            let lines = batch.get(i, Move::LINE_IDS)?;
            let sum: i32 = batch
                .values_of(MoveLine::QUANTITY, lines.ids())?
                .iter()
                .sum();
            batch.set(i, Move::QUANTITY, sum)?;
        }
        Ok(())
    }
}

/// `test_orm.move_line`.
#[model(
    name = "test_dep.move_line",
    description = "Dep Move Line",
    addon = "test_depends"
)]
pub struct MoveLine {
    #[field(many2one, comodel = "test_dep.move", required, ondelete = "cascade")]
    pub move_id: Many2one<Move>,
    #[field(boolean, default = true)]
    pub visible: bool,
    #[field(integer)]
    pub quantity: i32,
}

/// `test_orm.model_parent_m2o`.
#[model(
    name = "test_dep.parent_m2o",
    description = "Dep Parent M2O",
    addon = "test_depends"
)]
pub struct ParentM2o {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(one2many, comodel = "test_dep.child_m2o", inverse_name = "parent_id")]
    pub child_ids: X2many<ChildM2o>,
}

/// `test_orm.model_child_m2o`.
#[model(
    name = "test_dep.child_m2o",
    description = "Dep Child M2O",
    addon = "test_depends"
)]
pub struct ChildM2o {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_dep.parent_m2o", ondelete = "cascade")]
    pub parent_id: Many2one<ParentM2o>,
}

/// `test_orm.multi` (subset).
#[model(
    name = "test_dep.multi",
    description = "Dep Multi",
    addon = "test_depends"
)]
pub struct Multi {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(one2many, comodel = "test_dep.multi_line", inverse_name = "multi")]
    pub lines: X2many<MultiLine>,
}

/// `test_orm.multi.line` (subset).
#[model(
    name = "test_dep.multi_line",
    description = "Dep Multi Line",
    addon = "test_depends"
)]
pub struct MultiLine {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_dep.multi", ondelete = "cascade")]
    pub multi: Many2one<Multi>,
    #[field(many2many, comodel = "test_dep.multi_tag")]
    pub tags: X2many<MultiTag>,
}

/// `test_orm.multi.tag`.
#[model(
    name = "test_dep.multi_tag",
    description = "Dep Multi Tag",
    addon = "test_depends"
)]
pub struct MultiTag {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `test_orm.computed_inverse_one2many`: two non-stored computed one2many fields (lines by
/// priority) whose inverse rewrites `all_line_ids` as their union.
#[model(
    name = "test_dep.inverse_parent",
    description = "Dep Computed Inverse One2many",
    addon = "test_depends"
)]
pub struct InverseParent {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        one2many,
        comodel = "test_dep.inverse_line",
        inverse_name = "parent_id"
    )]
    pub all_line_ids: X2many<InverseLine>,
    #[field(
        one2many,
        comodel = "test_dep.inverse_line",
        inverse_name = "parent_id",
        compute = "_compute_priority_line_ids",
        inverse = "_inverse_line_ids"
    )]
    pub low_priority_line_ids: X2many<InverseLine>,
    #[field(
        one2many,
        comodel = "test_dep.inverse_line",
        inverse_name = "parent_id",
        compute = "_compute_priority_line_ids",
        inverse = "_inverse_line_ids"
    )]
    pub high_priority_line_ids: X2many<InverseLine>,
}

#[methods(InverseParent, addon = "test_depends")]
impl InverseParent {
    #[compute(depends = ["all_line_ids", "all_line_ids.priority"])]
    fn _compute_priority_line_ids(batch: &mut Batch<'_, InverseParent>) -> Result<()> {
        for i in batch.indices() {
            let lines = batch.get(i, InverseParent::ALL_LINE_IDS)?;
            let priorities = batch.values_of(InverseLine::PRIORITY, lines.ids())?;
            let mut low = Vec::new();
            let mut high = Vec::new();
            for (id, priority) in lines.ids().iter().zip(priorities) {
                if priority < 4 {
                    low.push(*id);
                } else {
                    high.push(*id);
                }
            }
            batch.set(i, InverseParent::LOW_PRIORITY_LINE_IDS, X2many::new(low))?;
            batch.set(i, InverseParent::HIGH_PRIORITY_LINE_IDS, X2many::new(high))?;
        }
        Ok(())
    }

    #[method]
    async fn _inverse_line_ids(
        this: &RecordSet<InverseParent>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for record in this.iter() {
            let low = record
                .related(tx, InverseParent::LOW_PRIORITY_LINE_IDS)
                .await?;
            let high = record
                .related(tx, InverseParent::HIGH_PRIORITY_LINE_IDS)
                .await?;
            let all = low.union(&high)?;
            record
                .as_records()
                .assign(tx, "all_line_ids", all.as_records())
                .await?;
        }
        Ok(())
    }
}

/// A line of [`InverseParent`].
#[model(
    name = "test_dep.inverse_line",
    description = "Dep Computed Inverse One2many Line",
    addon = "test_depends"
)]
pub struct InverseLine {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub priority: i32,
    #[field(many2one, comodel = "test_dep.inverse_parent")]
    pub parent_id: Many2one<InverseParent>,
}

// ----- parent store (…058) ----------------------------------------------

/// `test_orm.category` (subset): a `_parent_store` tree on `parent`, with `depth` computed from
/// the path.
#[model(
    name = "test_dep.category",
    description = "Dep Category",
    order = "name",
    parent_store = true,
    parent_name = "parent",
    addon = "test_depends"
)]
pub struct Category {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_dep.category", ondelete = "cascade")]
    pub parent: Many2one<Category>,
    #[field(char, index)]
    pub parent_path: Option<Arc<str>>,
    #[field(integer, compute = "_compute_depth")]
    pub depth: i32,
    /// Points at a category: `child_of` through another model's field.
    #[field(
        one2many,
        comodel = "test_dep.categorized",
        inverse_name = "category_id"
    )]
    pub item_ids: X2many<Categorized>,
}

#[methods(Category, addon = "test_depends")]
impl Category {
    /// Constraints of `create` see the final paths (CRUD §4.11).
    #[constrains("parent")]
    async fn _check_parent_path(this: &RecordSet<Category>, tx: &mut Transaction) -> Result<()> {
        for rec in this.iter() {
            let path = rec.get(tx, Category::PARENT_PATH).await?;
            let own = rec.ids().first().copied().unwrap_or_default();
            if !path
                .as_deref()
                .is_some_and(|p| p.ends_with(&format!("/{own}/")) || p == format!("{own}/"))
            {
                return Err(Error::validation(format!(
                    "category {own}: parent_path {path:?} does not end with its id"
                )));
            }
        }
        Ok(())
    }

    #[compute(depends = ["parent_path"])]
    fn _compute_depth(batch: &mut Batch<'_, Category>) -> Result<()> {
        for i in batch.indices() {
            let path = batch.get(i, Category::PARENT_PATH)?;
            let slashes = path.as_deref().unwrap_or("").matches('/').count();
            batch.set(
                i,
                Category::DEPTH,
                i32::try_from(slashes).unwrap_or(i32::MAX) - 1,
            )?;
        }
        Ok(())
    }
}

/// Records classified in a category.
#[model(
    name = "test_dep.categorized",
    description = "Dep Categorized",
    addon = "test_depends"
)]
pub struct Categorized {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_dep.category")]
    pub category_id: Many2one<Category>,
}

// ----- prefetch through assignment -------------------------------------------------

/// `test_orm.prefetch` (subset).
#[model(
    name = "test_dep.prefetch",
    description = "Dep Prefetch",
    addon = "test_depends"
)]
pub struct Prefetch {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub harry: i32,
    #[field(
        one2many,
        comodel = "test_dep.prefetch_line",
        inverse_name = "prefetch_id"
    )]
    pub line_ids: X2many<PrefetchLine>,
}

/// `test_orm.prefetch.line`: a stored related field following the parent.
#[model(
    name = "test_dep.prefetch_line",
    description = "Dep Prefetch Line",
    addon = "test_depends"
)]
pub struct PrefetchLine {
    #[field(many2one, comodel = "test_dep.prefetch")]
    pub prefetch_id: Many2one<Prefetch>,
    #[field(integer, related = "prefetch_id.harry", store)]
    pub harry: i32,
}

// ----- S4b review: non-stored hops, mirrors with a domain, multi-hop recursion ----------------------

/// An order of [`HopLine`].
#[model(
    name = "test_dep.hop_order",
    description = "Dep Hop Order",
    addon = "test_depends"
)]
pub struct HopOrder {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A stored compute through a non-stored related many2one: the records to recompute are found
/// by searching that field.
#[model(
    name = "test_dep.hop_line",
    description = "Dep Hop Line",
    addon = "test_depends"
)]
pub struct HopLine {
    #[field(many2one, comodel = "test_dep.hop_order")]
    pub order_id: Many2one<HopOrder>,
    #[field(many2one, comodel = "test_dep.hop_order", related = "order_id")]
    pub related_order_id: Many2one<HopOrder>,
    #[field(char, compute = "_compute_order_name", store)]
    pub order_name: Option<Arc<str>>,
}

#[methods(HopLine, addon = "test_depends")]
impl HopLine {
    #[compute(depends = ["related_order_id.name"])]
    fn _compute_order_name(batch: &mut Batch<'_, HopLine>) -> Result<()> {
        for i in batch.indices() {
            let name = match batch.get(i, HopLine::RELATED_ORDER_ID)?.id() {
                Some(o) => batch.value_of(HopOrder::NAME, o)?,
                None => None,
            };
            batch.set(i, HopLine::ORDER_NAME, name)?;
        }
        Ok(())
    }
}

/// Records with labels and a stored copy of the labels' names.
#[model(
    name = "test_dep.tagged",
    description = "Dep Tagged",
    addon = "test_depends"
)]
pub struct Tagged {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_dep.label",
        relation = "test_dep_tagged_label_rel",
        column1 = "tagged_id",
        column2 = "label_id"
    )]
    pub label_ids: X2many<Label>,
    #[field(char, compute = "_compute_label_names", store)]
    pub label_names: Option<Arc<str>>,
}

#[methods(Tagged, addon = "test_depends")]
impl Tagged {
    #[compute(depends = ["label_ids.name"])]
    fn _compute_label_names(batch: &mut Batch<'_, Tagged>) -> Result<()> {
        for i in batch.indices() {
            let labels = batch.get(i, Tagged::LABEL_IDS)?;
            let names: Vec<String> = batch
                .values_of(Label::NAME, labels.ids())?
                .into_iter()
                .map(|n| n.as_deref().unwrap_or("").to_owned())
                .collect();
            batch.set(i, Tagged::LABEL_NAMES, names.join(",").as_str())?;
        }
        Ok(())
    }
}

/// A label whose mirror many2many has a domain: its cached value does not list every owner.
#[model(
    name = "test_dep.label",
    description = "Dep Label",
    addon = "test_depends"
)]
pub struct Label {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_dep.tagged",
        relation = "test_dep_tagged_label_rel",
        column1 = "label_id",
        column2 = "tagged_id",
        domain = [("name", "!=", "hidden")]
    )]
    pub tagged_ids: X2many<Tagged>,
}

/// A recursive stored field through two hops (`link_id.chain_id.level`): computed level by level.
#[model(
    name = "test_dep.chain",
    description = "Dep Chain",
    addon = "test_depends"
)]
pub struct Chain {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_dep.chain_link")]
    pub link_id: Many2one<ChainLink>,
    #[field(integer, compute = "_compute_level", store)]
    pub level: i32,
}

#[methods(Chain, addon = "test_depends")]
impl Chain {
    #[compute(depends = ["link_id.chain_id.level"])]
    fn _compute_level(batch: &mut Batch<'_, Chain>) -> Result<()> {
        for i in batch.indices() {
            let above = match batch.get(i, Chain::LINK_ID)?.id() {
                Some(link) => match batch.value_of(ChainLink::CHAIN_ID, link)?.id() {
                    Some(chain) => Some(batch.value_of(Chain::LEVEL, chain)?),
                    None => None,
                },
                None => None,
            };
            batch.set(i, Chain::LEVEL, above.map_or(0, |l| l + 1))?;
        }
        Ok(())
    }
}

/// The hop between two [`Chain`] records.
#[model(
    name = "test_dep.chain_link",
    description = "Dep Chain Link",
    addon = "test_depends"
)]
pub struct ChainLink {
    #[field(many2one, comodel = "test_dep.chain")]
    pub chain_id: Many2one<Chain>,
}
