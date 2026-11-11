//! Fixtures: typed
//! exclusions over two-field periods, range fields and a `WITHOUT OVERLAPS` key.
//
//

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_scale_excl");

/// `scale.resource`: what bookings and rates are about.
#[model(
    name = "scale.resource",
    description = "Resource",
    addon = "test_scale_excl"
)]
pub struct Resource {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
}

/// `scale.booking`: no two bookings of a resource overlap, cancelled ones excepted
/// . A booking without a stop is open-ended.
#[model(
    name = "scale.booking",
    description = "Booking",
    addon = "test_scale_excl",
    exclusions = [exclude(
        name = "_no_double_booking",
        equal = ["resource_id"],
        period = ("start", "stop"),
        bounds = "[)",
        filter = "[('state', '!=', 'cancel')]",
        message = "This resource is already booked for an overlapping period."
    )]
)]
pub struct Booking {
    #[field(many2one, comodel = "scale.resource", required)]
    pub resource_id: Option<RecId>,
    #[field(datetime, string = "Start")]
    pub start: Option<DateTime>,
    #[field(datetime, string = "Stop")]
    pub stop: Option<DateTime>,
    #[field(
        selection,
        selection = [("confirmed", "Confirmed"), ("cancel", "Cancelled")],
        default = "confirmed",
        required
    )]
    pub state: Option<Arc<str>>,
}

/// `scale.stay`: date periods that include both ends (`[]`), so two stays that share a day
/// overlap, and a stay of one day (`arrival = departure`) is valid.
#[model(
    name = "scale.stay",
    description = "Stay",
    addon = "test_scale_excl",
    exclusions = [exclude(
        name = "_no_double_stay",
        equal = ["room"],
        period = ("arrival", "departure"),
        bounds = "[]",
        message = "This room is already taken on one of these days."
    )]
)]
pub struct Stay {
    #[field(integer, required)]
    pub room: i32,
    #[field(date, string = "Arrival")]
    pub arrival: Option<Date>,
    #[field(date, string = "Departure")]
    pub departure: Option<Date>,
}

/// `scale.rate`: one rate per product for any date.
#[model(
    name = "scale.rate",
    description = "Rate",
    addon = "test_scale_excl",
    keys = [key(
        name = "_rate_key",
        fields = ["product_id"],
        without_overlaps = "validity",
        message = "A product has one rate for any date."
    )]
)]
pub struct Rate {
    #[field(many2one, comodel = "scale.resource", required)]
    pub product_id: Option<RecId>,
    #[field(date_range, bounds = "[)", string = "Validity")]
    pub validity: Option<DateRange>,
    #[field(float)]
    pub amount: f64,
}

/// `scale.window`: a `datetime_range` and a `date_range` field that include both ends
/// (`[]` dates are stored as `[)` by PostgreSQL and read back as declared).
#[model(
    name = "scale.window",
    description = "Window",
    addon = "test_scale_excl"
)]
pub struct Window {
    #[field(integer)]
    pub room: i32,
    #[field(datetime_range, bounds = "[]", string = "Opening")]
    pub opening: Option<DatetimeRange>,
    #[field(date_range, bounds = "[]", string = "Days")]
    pub days: Option<DateRange>,
}

/// A native raw `EXCLUDE` table object that states the extension it needs: its
/// own addon, so a test can install it alone.
pub mod raw {
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_scale_excl_raw");

    /// `scale.raw_booking`.
    #[model(
        name = "scale.raw_booking",
        description = "Raw booking",
        addon = "test_scale_excl_raw",
        extensions = ["btree_gist"],
        constraints = [(
            "_raw_no_overlap",
            "EXCLUDE USING gist (resource_id WITH =, tsrange(start, stop) WITH &&)",
            "This resource is already booked (raw constraint)."
        )]
    )]
    pub struct RawBooking {
        #[field(integer)]
        pub resource_id: i32,
        #[field(datetime)]
        pub start: Option<DateTime>,
        #[field(datetime)]
        pub stop: Option<DateTime>,
    }
}
