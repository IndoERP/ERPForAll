//! Fixtures of the tracked-field capture (
//! ): `track.task` with one tracked field per scalar kind, a tracked many2one
//! and many2many, and an untracked `note`; `track.stock` with a business key to exercise upsert.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_tracking");

/// `track.partner`: what a task's tracked many2one points at.
#[model(
    name = "track.partner",
    description = "Tracking partner",
    addon = "test_tracking"
)]
pub struct Partner {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `track.tag`: what a task's tracked many2many links.
#[model(
    name = "track.tag",
    description = "Tracking tag",
    addon = "test_tracking"
)]
pub struct Tag {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// `track.task`: tracked fields with growing sequences (`tracking = true` is 100 and sorts last);
/// `note` is not tracked.
#[model(
    name = "track.task",
    description = "Tracking task",
    addon = "test_tracking"
)]
pub struct Task {
    #[field(char, tracking = true)]
    pub name: Option<Arc<str>>,
    #[field(selection, selection = [("draft", "Draft"), ("done", "Done")], tracking = 20)]
    pub state: Option<Arc<str>>,
    #[field(integer, tracking = 30)]
    pub qty: i32,
    #[field(float, tracking = 35)]
    pub weight: f64,
    #[field(many2one, comodel = "track.partner", tracking = 40)]
    pub partner_id: Option<RecId>,
    #[field(boolean, tracking = 50)]
    pub flagged: bool,
    #[field(date, tracking = 60)]
    pub due: Option<Date>,
    #[field(datetime, tracking = 70)]
    pub seen: Option<DateTime>,
    #[field(many2many, comodel = "track.tag", tracking = 80)]
    pub tag_ids: Vec<RecId>,
    #[field(char)]
    pub note: Option<Arc<str>>,
}

/// `track.stock`: a business key and a tracked quantity (an upsert that updates is a tracked write).
#[model(
    name = "track.stock",
    description = "Tracking stock",
    addon = "test_tracking",
    keys = [key(name = "_code_key", fields = ["code"], message = "The code must be unique.")]
)]
pub struct Stock {
    #[field(char, required)]
    pub code: Option<Arc<str>>,
    #[field(integer, tracking = 10)]
    pub qty: i32,
    /// A float column (r1: float old/new representation).
    #[field(float, tracking = 11)]
    pub ratio: f64,
    /// A fixed-scale decimal column: the database rounds a written value to two places (r1 M2).
    #[field(float, digits = (0, 2), tracking = 12)]
    pub price: Decimal,
}

/// `track.child`: delegates to `track.task` (`_inherits`); the delegated `name` is tracked on the
/// parent only.
#[model(
    name = "track.child",
    description = "Tracking child",
    inherits = [("track.task", "task_id")],
    addon = "test_tracking"
)]
pub struct Child {
    #[field(many2one, comodel = "track.task", required, ondelete = "cascade")]
    pub task_id: Many2one<Task>,
    #[field(char)]
    pub extra: Option<Arc<str>>,
}
