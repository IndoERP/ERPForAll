//! Partitioned leaf models: `scale.event` keyed on
//! `create_date` by day, `scale.audit` keyed on a `server_set` field by month and kept forever, and
//! `scale.note`, which points to events only softly (`res_model`/`res_id`).

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_partition");

/// `scale.event`: daily partitions on `create_date`, two days ahead, three days kept
/// ('s window).
#[model(
    name = "scale.event",
    description = "Scale Event",
    addon = "test_partition",
    partition = partition(interval = "day", premake = 2, retain = 3)
)]
pub struct Event {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `scale.audit`: monthly partitions on the server-set `recorded_at`, every partition kept.
#[model(
    name = "scale.audit",
    description = "Scale Audit",
    addon = "test_partition",
    partition = partition(by = "recorded_at", interval = "month", premake = 2, retain = "all")
)]
pub struct Audit {
    #[field(datetime, required, server_set, string = "Recorded at")]
    pub recorded_at: Option<DateTime>,
    #[field(char)]
    pub what: Option<Arc<str>>,
}

/// `scale.note`: a soft reference to any record (allowed towards a partitioned model).
#[model(
    name = "scale.note",
    description = "Scale Note",
    addon = "test_partition"
)]
pub struct Note {
    #[field(char)]
    pub res_model: Option<Arc<str>>,
    #[field(many2one_reference, model_field = "res_model")]
    pub res_id: i64,
}
