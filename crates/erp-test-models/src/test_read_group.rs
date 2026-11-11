use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_read_group");

/// `Pt`: the partner of fixture A, ordered `name ASC, id DESC`.
#[model(
    name = "test_read_group.partner",
    description = "Read Group Partner",
    order = "name ASC, id DESC",
    addon = "test_read_group"
)]
pub struct Partner {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub email: Option<Arc<str>>,
    #[field(boolean)]
    pub fold: bool,
}

/// Fixture A (`test_read_group.aggregate`).
#[model(
    name = "test_read_group.aggregate",
    description = "Group Test Aggregate",
    order = "id",
    addon = "test_read_group"
)]
pub struct Aggregate {
    #[field(integer)]
    pub key: i32,
    #[field(integer)]
    pub value: i32,
    #[field(float, digits = (10, 2))]
    pub numeric_value: Decimal,
    #[field(boolean)]
    pub bool_and: bool,
    #[field(many2one, comodel = "test_read_group.partner")]
    pub partner_id: Option<RecId>,
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_read_group.aggregate")]
    pub parent_id: Option<RecId>,
}

/// Users of fixture T.
#[model(
    name = "test_read_group.user",
    description = "Read Group User",
    addon = "test_read_group"
)]
pub struct User {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// Tags (with an active field) of fixture T.
#[model(
    name = "test_read_group.tag",
    description = "Read Group Tag",
    addon = "test_read_group"
)]
pub struct Tag {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
}

/// Fixture T (`test_read_group.task`).
#[model(
    name = "test_read_group.task",
    description = "Read Group Task",
    addon = "test_read_group"
)]
pub struct Task {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub key: i32,
    #[field(integer)]
    pub integer: i32,
    #[field(
        many2many,
        comodel = "test_read_group.user",
        relation = "test_read_group_task_user_rel",
        column1 = "task_id",
        column2 = "user_id"
    )]
    pub user_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_read_group.user",
        relation = "test_read_group_task_customer_rel",
        column1 = "task_id",
        column2 = "user_id"
    )]
    pub customer_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_read_group.tag",
        relation = "test_read_group_task_tag_rel",
        column1 = "task_id",
        column2 = "tag_id"
    )]
    pub tag_ids: Vec<RecId>,
    #[field(
        many2many,
        comodel = "test_read_group.tag",
        relation = "test_read_group_task_tag_rel",
        column1 = "task_id",
        column2 = "tag_id",
        context = "{\"active_test\": false}"
    )]
    pub all_tag_ids: Vec<RecId>,
}

/// Orders/-020/-056 (`test_read_group.order`).
#[model(
    name = "test_read_group.order",
    description = "Read Group Order",
    addon = "test_read_group"
)]
pub struct Order {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_read_group.order")]
    pub many2one_id: Option<RecId>,
    #[field(
        one2many,
        comodel = "test_read_group.order.line",
        inverse_name = "order_id"
    )]
    pub line_ids: Vec<RecId>,
}

/// Lines of the orders (`test_read_group.order.line`).
#[model(
    name = "test_read_group.order.line",
    description = "Read Group Order Line",
    addon = "test_read_group"
)]
pub struct OrderLine {
    #[field(many2one, comodel = "test_read_group.order", ondelete = "cascade")]
    pub order_id: Option<RecId>,
    #[field(integer)]
    pub value: i32,
    #[field(char)]
    pub label: Option<Arc<str>>,
}

/// Fixture F (`test_read_group.fill_temporal`).
#[model(
    name = "test_read_group.fill_temporal",
    description = "Read Group Fill Temporal",
    addon = "test_read_group"
)]
pub struct FillTemporal {
    #[field(date)]
    pub date: Option<Date>,
    #[field(datetime)]
    pub datetime: Option<DateTime>,
    #[field(integer)]
    pub value: i32,
}

/// Selection ordering and nullable booleans.
#[model(
    name = "test_read_group.selection",
    description = "Read Group Selection",
    addon = "test_read_group"
)]
pub struct SelectionModel {
    #[field(selection, selection = [("c", "C"), ("b", "B"), ("a", "A")])]
    pub sel: Option<Arc<str>>,
    #[field(boolean)]
    pub flag: bool,
}

/// `bar` of the base → foo → bar chain.
#[model(
    name = "test_read_group.bar",
    description = "Read Group Bar",
    addon = "test_read_group"
)]
pub struct Bar {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `foo` of the chain.
#[model(
    name = "test_read_group.foo",
    description = "Read Group Foo",
    addon = "test_read_group"
)]
pub struct Foo {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_read_group.bar")]
    pub bar_id: Option<RecId>,
}

/// `base` of the chain.
#[model(
    name = "test_read_group.chain",
    description = "Read Group Chain",
    addon = "test_read_group"
)]
pub struct Chain {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_read_group.foo")]
    pub foo_id: Option<RecId>,
}

/// `test_read_group.related_base`.
#[model(
    name = "test_read_group.related_base",
    description = "Read Group Related Base",
    addon = "test_read_group"
)]
pub struct RelatedBase {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(integer)]
    pub value: i32,
}

/// `test_read_group.related_inherits`, delegating to `related_base`.
#[model(
    name = "test_read_group.related_inherits",
    description = "Read Group Related Inherits",
    addon = "test_read_group",
    inherits = [("test_read_group.related_base", "base_id")]
)]
pub struct RelatedInherits {
    #[field(
        many2one,
        comodel = "test_read_group.related_base",
        required,
        ondelete = "cascade"
    )]
    pub base_id: Option<RecId>,
}

/// A model whose `_read_group` override answers another request (review item 7: refused).
#[model(
    name = "test_read_group.bad_override",
    description = "Read Group Bad Override",
    addon = "test_read_group"
)]
pub struct BadOverride {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(BadOverride, addon = "test_read_group")]
impl BadOverride {
    /// Always one `__count` group, whatever was asked.
    #[method(override)]
    async fn _read_group(
        _this: &RecordSet<BadOverride>,
        _tx: &mut Transaction,
        _args: ReadGroup,
        _sup: Super,
    ) -> Result<Groups> {
        Groups::from_rows(
            Vec::new(),
            vec!["__count".to_owned()],
            vec![vec![GroupValue::Int(1)]],
        )
    }
}
