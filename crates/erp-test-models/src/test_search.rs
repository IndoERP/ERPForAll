use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_search");

/// Like `test_orm.partner` (scalar fields plus a raw many2one).
#[model(
    name = "test_search.partner",
    description = "Search Partner",
    addon = "test_search"
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
    #[field(integer)]
    pub number: i32,
    #[field(many2one, comodel = "test_search.country")]
    pub country_id: Option<RecId>,
}

/// A comodel ordered by `id` (many2one ordering by the raw column).
#[model(
    name = "test_search.country",
    description = "Search Country",
    addon = "test_search"
)]
pub struct Country {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub code: Option<Arc<str>>,
}

/// One field per scalar type, all nullable (NULL semantics per type).
#[model(
    name = "test_search.mixed",
    description = "Search Mixed",
    addon = "test_search"
)]
pub struct Mixed {
    #[field(integer)]
    pub number: i32,
    #[field(float)]
    pub x: f64,
    #[field(float, digits = 0)]
    pub amount: Decimal,
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(date)]
    pub date: Option<Date>,
    #[field(datetime)]
    pub moment: Option<DateTime>,
    #[field(boolean)]
    pub flag: bool,
    #[field(selection, selection = [("c", "C"), ("b", "B"), ("a", "A")])]
    pub state: Option<Arc<str>>,
    #[field(many2one, comodel = "test_search.mixed")]
    pub parent_id: Option<RecId>,
}

/// Name search over two fields (`ir.model`'s `_rec_names_search = ('name', 'model')`), ordered by
/// `code`.
#[model(
    name = "test_search.named",
    description = "Search Named",
    order = "code",
    rec_names_search = ["name", "code"],
    addon = "test_search"
)]
pub struct Named {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub code: Option<Arc<str>>,
    #[field(integer)]
    pub sequence: i32,
}

/// No rec name: name search does not restrict anything.
#[model(
    name = "test_search.anonymous",
    description = "Search Anonymous",
    addon = "test_search"
)]
pub struct Anonymous {
    #[field(integer)]
    pub size: i32,
}

/// Only a manual `x_active` field.
#[model(
    name = "test_search.x_active",
    description = "Search X Active",
    addon = "test_search"
)]
pub struct XActive {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub x_active: bool,
}

/// Both `active` and `x_active`: `active` drives the filter.
#[model(
    name = "test_search.both_active",
    description = "Search Both Active",
    addon = "test_search"
)]
pub struct BothActive {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(boolean, default = true)]
    pub x_active: bool,
}

/// A model whose rec name is a many2one (its label needs the comodel, slice S3).
#[model(
    name = "test_search.by_partner",
    description = "Search By Partner",
    rec_name = "partner_id",
    addon = "test_search"
)]
pub struct ByPartner {
    #[field(many2one, comodel = "test_search.partner")]
    pub partner_id: Option<RecId>,
}

/// Like `test_performance.base` (scalar parts): query budgets of search, fetch and name search.
#[model(
    name = "test_search.perf",
    description = "Search Performance Base",
    addon = "test_search"
)]
pub struct Perf {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer, default = 0)]
    pub value: i32,
    #[field(float)]
    pub value_pc: f64,
    #[field(many2one, comodel = "test_search.partner", string = "Customer")]
    pub partner_id: Option<RecId>,
    #[field(char, prefetch = false)]
    pub rare: Option<Arc<str>>,
}
