use erp_sdk::OrmCacheKey;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_rset");

/// `test_orm.country` (`_order = 'name, id'`).
#[model(
    name = "test_rset.country",
    description = "Country",
    order = "name, id",
    addon = "test_rset"
)]
pub struct Country {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `test_orm.city` (`_order = 'country_id, name, id'`): ordering by the model order compares
/// countries by their own order.
#[model(
    name = "test_rset.city",
    description = "City",
    order = "country_id, name, id",
    addon = "test_rset"
)]
pub struct City {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rset.country")]
    pub country_id: Option<RecId>,
    #[field(many2one, comodel = "test_rset.region")]
    pub region_id: Option<RecId>,
}

/// A comodel whose order starts with a descending term: SQL puts cities without a region (all
/// comodel columns NULL) first by default, so the in-memory sort must as well.
#[model(
    name = "test_rset.region",
    description = "Region",
    order = "name desc, id",
    addon = "test_rset"
)]
pub struct Region {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `test_orm.category` (`_order = 'name'`, many2one `parent` to itself).
#[model(
    name = "test_rset.category",
    description = "Category",
    order = "name",
    addon = "test_rset"
)]
pub struct Category {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rset.category")]
    pub parent: Option<RecId>,
}

/// A model whose `_order` starts with its own parent many2one: expanding the comodel order meets
/// the same many2one again (the recursion guard).
#[model(
    name = "test_rset.tree",
    description = "Tree",
    order = "parent_id, name",
    addon = "test_rset"
)]
pub struct Tree {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "test_rset.tree")]
    pub parent_id: Option<RecId>,
}

/// Scalar fields of every kind the in-memory operations compare.
#[model(
    name = "test_rset.partner",
    description = "Partner",
    addon = "test_rset"
)]
pub struct Partner {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(char)]
    pub email: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(integer)]
    pub number: i32,
    #[field(float)]
    pub amount: f64,
    #[field(float, digits = (16, 2))]
    pub price: Decimal,
    #[field(selection, selection = [("draft", "Draft"), ("open", "Open"), ("cancel", "Cancel")])]
    pub state: Option<Arc<str>>,
    #[field(date)]
    pub date: Option<Date>,
    #[field(datetime)]
    pub moment: Option<DateTime>,
    #[field(json)]
    pub data: Option<erp_sdk::serde_json::Value>,
    #[field(many2one, comodel = "test_rset.country")]
    pub country_id: Option<RecId>,
    /// Non-stored computed: `name` in upper case (filtered in memory, never in SQL).
    #[field(char, compute = "_compute_upper")]
    pub upper: Option<Arc<str>>,
}

#[methods(Partner, addon = "test_rset")]
impl Partner {
    #[compute(depends = ["name"])]
    fn _compute_upper(batch: &mut Batch<'_, Partner>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, Partner::NAME)?;
            batch.set(
                i,
                Partner::UPPER,
                name.map(|n| Arc::<str>::from(n.to_uppercase())),
            )?;
        }
        Ok(())
    }
}

/// Like `ir.actions.actions`: `exists()` answers from an ormcached set of every id instead of
/// querying, so a row deleted behind the ORM's back is reported until the cache is cleared.
#[model(name = "test_rset.action", description = "Action", addon = "test_rset")]
pub struct Action {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// The ormcache holding every action id.
pub const ACTION_IDS_CACHE: &str = "test_rset.action.ids";

#[methods(Action, addon = "test_rset")]
impl Action {
    #[method(override)]
    async fn exists(this: &RecordSet<Action>, tx: &mut Transaction, sup: Super) -> Result<Records> {
        let _ = sup;
        let key = OrmCacheKey::new("test_rset.action", "_existing", Vec::new());
        let all = tx
            .ormcache(ACTION_IDS_CACHE, key, async |tx: &mut Transaction| {
                let all = tx
                    .env()
                    .model::<Action>()?
                    .search(tx, &Domain::TRUE)
                    .await?;
                Ok(Value::list(all.ids().into_iter().map(Value::Int)))
            })
            .await?;
        let known: Vec<i64> = match all {
            Value::List(items) => items
                .iter()
                .filter_map(|v| match v {
                    Value::Int(i) => Some(*i),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        Ok(this
            .as_records()
            .filtered_by(|r| r.ids().first().is_some_and(|id| known.contains(id))))
    }
}

/// Like `analytic.mixin`: `filtered_domain` rewrites conditions on `tag` into conditions on
/// `tag_code` before delegating to `super()`.
#[model(name = "test_rset.line", description = "Line", addon = "test_rset")]
pub struct Line {
    #[field(char)]
    pub tag: Option<Arc<str>>,
    #[field(char)]
    pub tag_code: Option<Arc<str>>,
}

#[methods(Line, addon = "test_rset")]
impl Line {
    #[method(override)]
    async fn filtered_domain(
        this: &RecordSet<Line>,
        tx: &mut Transaction,
        domain: Domain,
        sup: Super,
    ) -> Result<Records> {
        let rewritten = domain.map_conditions(|node| match node.as_condition() {
            Some(c) if c.field_expr() == "tag" => {
                Domain::condition("tag_code", c.operator(), c.value().clone())
                    .unwrap_or_else(|_| node.clone())
            }
            _ => node.clone(),
        });
        sup.call(tx, this.as_records(), (rewritten,)).await
    }
}
