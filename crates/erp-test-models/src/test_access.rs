use erp_sdk::access::{AccessCtx, AccessSuper, Operation};
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_access", depends = ["base_users"]);

/// A company (the fixture's `company_ids` rules compare with its ids).
#[model(
    name = "test_access.company",
    description = "Access Company",
    addon = "test_access"
)]
pub struct Company {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A partner with a commercial partner and a hierarchy (`child_of`/`parent_of`).
#[model(
    name = "test_access.partner",
    description = "Access Partner",
    addon = "test_access",
    parent_name = "parent_id"
)]
pub struct Partner {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_access.partner")]
    pub commercial_partner_id: Option<RecId>,
    #[field(many2one, comodel = "test_access.partner")]
    pub parent_id: Option<RecId>,
    #[field(one2many, comodel = "test_access.partner", inverse_name = "parent_id")]
    pub child_ids: Vec<RecId>,
}

/// The users' partner (a user path `user.access_partner_id`) and the group the tests' resolver
/// reads as the user's membership.
#[extend(
    ResUsers,
    addon = "test_access",
    signal_triggers = [(
        "_test_membership_bump",
        "membership",
        update_of = ["access_group_id"]
    )],
)]
pub struct UsersAccess {
    #[field(many2one, comodel = "test_access.partner")]
    pub access_partner_id: Option<RecId>,
    #[field(many2one, comodel = "test_access.group")]
    pub access_group_id: Option<RecId>,
    /// The user's organisation (a user path `user.access_org_id.id` into a `_parent_store` tree).
    #[field(many2one, comodel = "test_access.org")]
    pub access_org_id: Option<RecId>,
}

/// An organisation tree with `_parent_store` (`child_of` by `parent_path` ranges).
#[model(
    name = "test_access.org",
    description = "Access Org",
    addon = "test_access",
    parent_store = true,
    parent_name = "parent_id"
)]
pub struct Org {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_access.org")]
    pub parent_id: Option<RecId>,
    #[field(char, index)]
    pub parent_path: Option<Arc<str>>,
}

#[model(
    name = "test_access.org_doc",
    description = "Access Org Document",
    addon = "test_access"
)]
pub struct OrgDoc {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_access.org")]
    pub org_id: Option<RecId>,
    #[field(
        many2many,
        comodel = "test_access.org",
        relation = "test_access_org_doc_follower_rel",
        column1 = "doc_id",
        column2 = "org_id"
    )]
    pub follower_org_ids: Vec<RecId>,
}

/// A group of the tests' resolver (its id is the `GroupId`).
#[model(
    name = "test_access.group",
    description = "Access Group",
    addon = "test_access"
)]
pub struct Group {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A policy-source row of the tests' policy provider: objects need `val > val_min` to be read.
#[model(
    name = "test_access.rule_row",
    description = "Access Rule Row",
    addon = "test_access"
)]
pub struct RuleRow {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub val_min: i32,
}

/// A category of objects.
#[model(
    name = "test_access.categ",
    description = "Access Category",
    addon = "test_access"
)]
pub struct Categ {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(Categ, addon = "test_access")]
impl Categ {
    /// Categories named `internal` are for the members of `test_access.group_manager` only.
    #[access_domain]
    fn _access_domain(ctx: &AccessCtx, op: Operation, sup: AccessSuper) -> Result<Domain> {
        let base = sup.call(ctx, op)?;
        if ctx.has_group("test_access.group_manager")? {
            return Ok(base);
        }
        Ok(base & Domain::condition("name", Operator::Ne, "internal")?)
    }
}

#[model(
    name = "test_access.some_obj",
    description = "Object For Test Access Right",
    addon = "test_access"
)]
pub struct SomeObj {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub val: i32,
    #[field(many2one, comodel = "test_access.company")]
    pub company_id: Option<RecId>,
    #[field(many2one, comodel = "res.users")]
    pub user_id: Option<RecId>,
    #[field(many2one, comodel = "test_access.partner")]
    pub partner_id: Option<RecId>,
    #[field(many2one, comodel = "test_access.categ")]
    pub categ_id: Option<RecId>,
    #[field(date)]
    pub deadline: Option<Date>,
    #[field(datetime)]
    pub moment: Option<DateTime>,
}

/// A model with a business key, for the upsert rows of the entry-point table.
#[model(
    name = "test_access.keyed",
    description = "Access Keyed Object",
    addon = "test_access",
    row_version = true,
    keys = [key(
        name = "_name_key",
        fields = ["name"],
        message = "A keyed object of this name already exists."
    )]
)]
pub struct Keyed {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub val: i32,
}

#[model(
    name = "test_access.container",
    description = "Access Container",
    addon = "test_access"
)]
pub struct Container {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_access.some_obj",
        relation = "test_access_container_some_rel",
        column1 = "container_id",
        column2 = "some_id"
    )]
    pub some_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_access.some_obj",
        relation = "test_access_container_bypass_rel",
        column1 = "container_id",
        column2 = "some_id",
        bypass_search_access
    )]
    pub bypass_ids: Vec<RecId>,
    #[field(one2many, comodel = "test_access.child", inverse_name = "container_id")]
    pub child_ids: Vec<RecId>,
}

#[model(
    name = "test_access.child",
    description = "Access Child",
    addon = "test_access"
)]
pub struct Child {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub val: i32,
    #[field(many2one, comodel = "test_access.container")]
    pub container_id: Option<RecId>,
}

/// A model with an `#[access_domain]` override: notes named `classified` are hidden from reads
/// whatever the rules say, outside superuser mode.
#[model(
    name = "test_access.note",
    description = "Access Note",
    addon = "test_access"
)]
pub struct Note {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(Note, addon = "test_access")]
impl Note {
    /// Narrows the read predicate of the policy (`super`).
    #[access_domain]
    fn _access_domain(ctx: &AccessCtx, op: Operation, sup: AccessSuper) -> Result<Domain> {
        let base = sup.call(ctx, op)?;
        if op != Operation::Read {
            return Ok(base);
        }
        Ok(base & Domain::condition("name", Operator::Ne, "classified")?)
    }
}

#[model(
    name = "test_access.related",
    description = "Access Related",
    addon = "test_access"
)]
pub struct Related {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_access.related_foo")]
    pub foo_id: Option<RecId>,
    #[field(char, related = "foo_id.bar_id.name", compute_sudo = false)]
    pub foo_bar_name: Option<Arc<str>>,
    #[field(char, related = "foo_id.bar_id.name")]
    pub foo_bar_name_sudo: Option<Arc<str>>,
    #[field(char, related = "foo_id.name", compute_sudo = false)]
    pub foo_name: Option<Arc<str>>,
    #[field(char, related = "foo_id.name")]
    pub foo_name_sudo: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "test_access.related_bar",
        related = "foo_id.bar_id",
        compute_sudo = false
    )]
    pub foo_bar_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "test_access.related_bar",
        related = "foo_id.bar_id"
    )]
    pub foo_bar_sudo_id: Option<RecId>,
}

/// `test_orm.related_foo`.
#[model(
    name = "test_access.related_foo",
    description = "Access Related Foo",
    addon = "test_access"
)]
pub struct RelatedFoo {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_access.related_bar")]
    pub bar_id: Option<RecId>,
}

/// `test_orm.related_bar`.
#[model(
    name = "test_access.related_bar",
    description = "Access Related Bar",
    addon = "test_access"
)]
pub struct RelatedBar {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A ledger whose columns change after its own INSERT when lines are given at create: `total`
/// depends on `line_ids`, so the dependency graph computes it after the INSERT (an x2many hop
/// has no values before it, whatever `precompute` says) and the inverse of `tag_ids` writes
/// `state` (create-check fusion tests).
#[model(
    name = "test_access.ledger",
    description = "Access Ledger",
    addon = "test_access"
)]
pub struct Ledger {
    #[field(char)]
    pub state: Option<Arc<str>>,
    /// Stored compute over `state` only: precomputed, so the INSERT already carries its value.
    #[field(char, compute = "_compute_shout", store)]
    pub shout: Option<Arc<str>>,
    /// Stored, editable, inversed: given at create, its inverse writes `state` after the INSERT.
    #[field(char, compute = "_compute_label", inverse = "_inverse_label", store)]
    pub label: Option<Arc<str>>,
    /// Stored compute over `line_ids` and their `val`: computed after the INSERT (the lines do not
    /// exist before it; declaring `precompute` on it is a registry error).
    #[field(integer, compute = "_compute_total", store)]
    pub total: i32,
    #[field(
        one2many,
        comodel = "test_access.ledger_line",
        inverse_name = "ledger_id"
    )]
    pub line_ids: X2many<LedgerLine>,
    #[field(
        one2many,
        comodel = "test_access.ledger_line",
        inverse_name = "ledger_id",
        compute = "_compute_tag_ids",
        inverse = "_inverse_tag_ids"
    )]
    pub tag_ids: X2many<LedgerLine>,
}

#[methods(Ledger, addon = "test_access")]
impl Ledger {
    #[compute(depends = ["line_ids", "line_ids.val"])]
    fn _compute_total(batch: &mut Batch<'_, Ledger>) -> Result<()> {
        for i in batch.indices() {
            let lines = batch.get(i, Ledger::LINE_IDS)?;
            let total: i32 = batch.values_of(LedgerLine::VAL, lines.ids())?.iter().sum();
            batch.set(i, Ledger::TOTAL, total)?;
        }
        Ok(())
    }

    #[compute(depends = ["state"])]
    fn _compute_shout(batch: &mut Batch<'_, Ledger>) -> Result<()> {
        for i in batch.indices() {
            let state = batch.get(i, Ledger::STATE)?;
            batch.set(
                i,
                Ledger::SHOUT,
                state.map(|s| Arc::<str>::from(s.to_uppercase())),
            )?;
        }
        Ok(())
    }

    #[compute(depends = ["state"])]
    fn _compute_label(batch: &mut Batch<'_, Ledger>) -> Result<()> {
        for i in batch.indices() {
            let state = batch.get(i, Ledger::STATE)?;
            batch.set(i, Ledger::LABEL, state)?;
        }
        Ok(())
    }

    /// Writing a label settles the ledger: it writes the record's own `state` column.
    #[method]
    async fn _inverse_label(this: &RecordSet<Ledger>, tx: &mut Transaction) -> Result<()> {
        for rec in this.iter() {
            rec.set(tx, Ledger::STATE, Some(Arc::<str>::from("done")))
                .await?;
        }
        Ok(())
    }

    #[compute(depends = ["line_ids"])]
    fn _compute_tag_ids(batch: &mut Batch<'_, Ledger>) -> Result<()> {
        for i in batch.indices() {
            let lines = batch.get(i, Ledger::LINE_IDS)?;
            batch.set(i, Ledger::TAG_IDS, lines)?;
        }
        Ok(())
    }

    /// Persisting the tags settles the ledger: it writes the record's own `state` column.
    #[method]
    async fn _inverse_tag_ids(this: &RecordSet<Ledger>, tx: &mut Transaction) -> Result<()> {
        for rec in this.iter() {
            rec.set(tx, Ledger::STATE, Some(Arc::<str>::from("done")))
                .await?;
        }
        Ok(())
    }
}

/// A line of [`Ledger`].
#[model(
    name = "test_access.ledger_line",
    description = "Access Ledger Line",
    addon = "test_access"
)]
pub struct LedgerLine {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub val: i32,
    #[field(many2one, comodel = "test_access.ledger")]
    pub ledger_id: Option<RecId>,
    /// Editable related to the ledger's state: given at create, it writes the ledger after the
    /// INSERT (COMP §4.8).
    #[field(char, related = "ledger_id.state", readonly = false)]
    pub ledger_state: Option<Arc<str>>,
    /// Stored compute over the ledger's state, through a many2one: precomputed into the INSERT,
    /// and recomputed when a related write of the same create changes the state.
    #[field(char, compute = "_compute_ledger_shout", store)]
    pub ledger_shout: Option<Arc<str>>,
}

#[methods(LedgerLine, addon = "test_access")]
impl LedgerLine {
    #[compute(depends = ["ledger_id.state"])]
    fn _compute_ledger_shout(batch: &mut Batch<'_, LedgerLine>) -> Result<()> {
        for i in batch.indices() {
            let state = match batch.get(i, LedgerLine::LEDGER_ID)? {
                Some(ledger) => batch.value_of(Ledger::STATE, ledger)?,
                None => None,
            };
            batch.set(
                i,
                LedgerLine::LEDGER_SHOUT,
                state.map(|s| Arc::<str>::from(s.to_uppercase())),
            )?;
        }
        Ok(())
    }
}

/// A team, readable by a rule over its members (the many2many memo cases).
#[model(
    name = "test_access.team",
    description = "Access Team",
    addon = "test_access"
)]
pub struct Team {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_access.member",
        relation = "test_access_team_member_rel",
        column1 = "team_id",
        column2 = "member_id"
    )]
    pub member_ids: Vec<RecId>,
    /// A second field over the same relation table (a rule may read either).
    #[field(
        many2many,
        comodel = "test_access.member",
        relation = "test_access_team_member_rel",
        column1 = "team_id",
        column2 = "member_id"
    )]
    pub alt_member_ids: Vec<RecId>,
}

/// A member of teams: the mirror many2many of [`Team::member_ids`] (same relation table).
#[model(
    name = "test_access.member",
    description = "Access Member",
    addon = "test_access"
)]
pub struct Member {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "test_access.team",
        relation = "test_access_team_member_rel",
        column1 = "member_id",
        column2 = "team_id"
    )]
    pub team_ids: Vec<RecId>,
}

/// The `_inherits` parent of [`Holder`].
#[model(
    name = "test_access.badge",
    description = "Access Badge",
    addon = "test_access"
)]
pub struct Badge {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A record with a delegated field and a field its read rule depends on.
#[model(
    name = "test_access.holder",
    description = "Access Holder",
    inherits = [("test_access.badge", "badge_id")],
    addon = "test_access"
)]
pub struct Holder {
    #[field(
        many2one,
        comodel = "test_access.badge",
        required,
        ondelete = "cascade"
    )]
    pub badge_id: Option<RecId>,
    #[field(integer)]
    pub level: i32,
}

/// A shelf readable by a rule over its books (an x2many traversal inside a rule, which must not
/// see the reader's `active_test`: SM-2 of the merge review).
#[model(
    name = "test_access.shelf",
    description = "Access Shelf",
    addon = "test_access"
)]
pub struct Shelf {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(one2many, comodel = "test_access.book", inverse_name = "shelf_id")]
    pub book_ids: Vec<RecId>,
}

/// A book of a shelf; archived books are the point of the shelf's rule.
#[model(
    name = "test_access.book",
    description = "Access Book",
    addon = "test_access"
)]
pub struct Book {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub val: i32,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(many2one, comodel = "test_access.shelf")]
    pub shelf_id: Option<RecId>,
}
