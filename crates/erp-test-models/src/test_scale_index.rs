//! Fixtures of the index kinds (design
//! ). The models are prefixed `scale.ix.` so they never meet the
//! fixtures of the other ORMX parts (`scale.event`, `scale.booking`, …) in one registry.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_scale_index");

/// `scale.ix.event`: an append-mostly log: `seq` and (through [`EventBrin`]) the audit field
/// `create_date` indexed with `brin`, a `json` payload with `gin`.
#[model(
    name = "scale.ix.event",
    description = "Scale index event",
    addon = "test_scale_index"
)]
pub struct Event {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(json, index = "gin")]
    pub payload: Option<erp_sdk::serde_json::Value>,
    #[field(integer, index = "brin")]
    pub seq: i32,
}

/// Gives the audit field `create_date` of [`Event`] a BRIN index (an extension may set `index` on
/// a field it does not declare, like any attribute).
#[extend(Event, addon = "test_scale_index")]
pub struct EventBrin {
    #[field(datetime, index = "brin")]
    pub create_date: Option<DateTime>,
}

/// `scale.ix.resource`: what a booking books.
#[model(
    name = "scale.ix.resource",
    description = "Scale index resource",
    addon = "test_scale_index"
)]
pub struct Resource {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `scale.ix.booking`: a typed covering index over `(state, start)` that carries `resource_id`,
/// for open bookings only (a constant filter).
#[model(
    name = "scale.ix.booking",
    description = "Scale index booking",
    addon = "test_scale_index",
    indexes = [index(
        name = "_open_by_date",
        fields = ["state", "start"],
        include = ["resource_id"],
        filter = "[('state', '=', 'open')]"
    )]
)]
pub struct Booking {
    #[field(many2one, comodel = "scale.ix.resource")]
    pub resource_id: Option<RecId>,
    #[field(datetime)]
    pub start: Option<DateTime>,
    #[field(datetime)]
    pub stop: Option<DateTime>,
    #[field(selection, selection = [("open", "Open"), ("cancel", "Cancelled")])]
    pub state: Option<Arc<str>>,
}

/// `scale.item`: a trigram index, which needs `pg_trgm`.
#[model(
    name = "scale.item",
    description = "Scale item",
    addon = "test_scale_index"
)]
pub struct Item {
    #[field(char, index = "trigram")]
    pub name: Option<Arc<str>>,
}

/// A second addon that adds typed indexes to [`Booking`], which it does not own, and replaces one
/// by name (design §2, §9: composed by full name, the later declaration wins).
pub mod ext {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(
        name = "test_scale_index_ext",
        depends = ["test_scale_index"]
    );

    /// The extension's indexes on `scale.ix.booking`.
    #[extend(
        super::Booking,
        addon = "test_scale_index_ext",
        indexes = [
            index(name = "_by_stop", fields = ["stop"], method = "brin"),
            index(name = "_open_by_date", fields = ["state", "start", "stop"], include = ["resource_id"]),
        ]
    )]
    pub struct BookingIndexes;
}
