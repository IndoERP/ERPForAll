//! Fixtures of the keys, upsert and row-version cases (
//! §4, and 033): `scale.quant` with the business key `_quant_key` over
//! `product_id, location_id, lot_id` (`lot_id` nullable) and a row version, its targets, and
//! `scale.serial` with a computed default. Extensions in separate addons add a stored compute, a
//! constraint and counting `create`/`write` overrides, and an `upsert` override
//! , so each test installs only what it exercises.

use std::sync::atomic::{AtomicI64, Ordering};

use erp_sdk::DefaultContext;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_scale_upsert");

/// `scale.company`: the company of a location (the policy fixture's write rule reads it).
#[model(
    name = "scale.company",
    description = "Scale Company",
    addon = "test_scale_upsert"
)]
pub struct ScaleCompany {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
}

/// `scale.location`.
#[model(
    name = "scale.location",
    description = "Scale Location",
    addon = "test_scale_upsert"
)]
pub struct ScaleLocation {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "scale.company")]
    pub company_id: Option<RecId>,
}

/// `scale.product`.
#[model(
    name = "scale.product",
    description = "Scale Product",
    addon = "test_scale_upsert"
)]
pub struct ScaleProduct {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
}

/// `scale.lot`.
#[model(
    name = "scale.lot",
    description = "Scale Lot",
    addon = "test_scale_upsert"
)]
pub struct ScaleLot {
    #[field(char, required)]
    pub name: Option<Arc<str>>,
}

/// `scale.quant`: one row per product, location and lot (no lot is a value too).
#[model(
    name = "scale.quant",
    description = "Scale Quant",
    addon = "test_scale_upsert",
    row_version = true,
    keys = [key(
        name = "_quant_key",
        fields = ["product_id", "location_id", "lot_id"],
        message = "A quant already exists for this product, location and lot."
    )]
)]
pub struct ScaleQuant {
    #[field(many2one, comodel = "scale.product", required)]
    pub product_id: Option<RecId>,
    #[field(many2one, comodel = "scale.location", required)]
    pub location_id: Option<RecId>,
    #[field(many2one, comodel = "scale.lot")]
    pub lot_id: Option<RecId>,
    #[field(float)]
    pub quantity: f64,
    #[field(float)]
    pub reserved_quantity: f64,
    #[field(integer)]
    pub move_count: i32,
    #[field(datetime)]
    pub in_date: Option<DateTime>,
    #[field(char)]
    pub note: Option<Arc<str>>,
}

/// Numbers handed out by [`next_serial_number`] (a sequence stand-in: the test reads it to see
/// that no number was consumed).
pub static SERIAL_NUMBERS: AtomicI64 = AtomicI64::new(0);

/// A computed default: the next number of a sequence.
fn next_serial_number(_ctx: &DefaultContext) -> Value {
    Value::Int(SERIAL_NUMBERS.fetch_add(1, Ordering::SeqCst) + 1)
}

/// `scale.serial`: a key over a text code, a computed default (`number`) and a static one
/// (`label`).
#[model(
    name = "scale.serial",
    description = "Scale Serial",
    addon = "test_scale_upsert",
    keys = [key(name = "_code_key", fields = ["code"], message = "The code must be unique.")]
)]
pub struct ScaleSerial {
    #[field(char, required)]
    pub code: Option<Arc<str>>,
    #[field(integer, default_fn = next_serial_number)]
    pub number: i32,
    #[field(char, default = "new")]
    pub label: Option<Arc<str>>,
    #[field(integer)]
    pub hits: i32,
}

/// and `create`/`write` overrides that count their calls.
pub mod hooks {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use erp_sdk::prelude::*;

    use super::{ScaleLocation, ScaleQuant};

    erp_sdk::addon!(
        name = "test_scale_upsert_hooks",
        depends = ["test_scale_upsert"]
    );

    /// Calls of the `create` override.
    pub static CREATE_CALLS: AtomicUsize = AtomicUsize::new(0);
    /// Calls of the `write` override.
    pub static WRITE_CALLS: AtomicUsize = AtomicUsize::new(0);
    /// Records the constraint checked, in order.
    pub static CHECKED: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());

    /// The extension.
    #[extend(ScaleQuant, addon = "test_scale_upsert_hooks")]
    pub struct QuantHooks {
        #[field(float, compute = "_compute_quantity_double", store)]
        pub quantity_double: f64,
        /// Recomputed when the location is renamed (a recompute-only `UPDATE`).
        #[field(char, compute = "_compute_location_name", store)]
        pub location_name: Option<Arc<str>>,
    }

    #[methods(ScaleQuant, addon = "test_scale_upsert_hooks")]
    impl QuantHooks {
        #[compute(depends = ["quantity"])]
        fn _compute_quantity_double(batch: &mut Batch<'_, ScaleQuant>) -> Result<()> {
            for i in batch.indices() {
                let quantity = batch.get(i, ScaleQuant::QUANTITY)?;
                batch.set(i, QuantHooks::QUANTITY_DOUBLE, quantity * 2.0)?;
            }
            Ok(())
        }

        #[compute(depends = ["location_id.name"])]
        fn _compute_location_name(batch: &mut Batch<'_, ScaleQuant>) -> Result<()> {
            for i in batch.indices() {
                let name = match batch.get(i, ScaleQuant::LOCATION_ID)? {
                    Some(location) => batch.value_of(ScaleLocation::NAME, location)?,
                    None => None,
                };
                batch.set(i, QuantHooks::LOCATION_NAME, name)?;
            }
            Ok(())
        }

        #[constrains("quantity")]
        async fn _check_quantity(this: &RecordSet<ScaleQuant>, tx: &mut Transaction) -> Result<()> {
            for rec in this.iter() {
                let quantity = rec.get(tx, ScaleQuant::QUANTITY).await?;
                CHECKED
                    .lock()
                    .map_err(|_| Error::programming("CHECKED lock poisoned"))?
                    .extend(rec.ids());
                if quantity > 1000.0 {
                    return Err(Error::validation(format!(
                        "A quant holds at most 1000 units (got {quantity})."
                    )));
                }
            }
            Ok(())
        }

        #[method(override)]
        async fn create(
            this: &RecordSet<ScaleQuant>,
            tx: &mut Transaction,
            vals_list: Vec<RawVals>,
            sup: Super,
        ) -> Result<Records> {
            CREATE_CALLS.fetch_add(1, Ordering::SeqCst);
            sup.call(tx, this.as_records(), (vals_list,)).await
        }

        #[method(override)]
        async fn write(
            this: &RecordSet<ScaleQuant>,
            tx: &mut Transaction,
            vals: RawVals,
            sup: Super,
        ) -> Result<()> {
            WRITE_CALLS.fetch_add(1, Ordering::SeqCst);
            sup.call(tx, this.as_records(), (vals,)).await
        }
    }
}

/// `super` and marks the returned records.
pub mod override_upsert {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use erp_sdk::prelude::*;

    use super::ScaleQuant;

    erp_sdk::addon!(
        name = "test_scale_upsert_override",
        depends = ["test_scale_upsert"]
    );

    /// Calls of the override.
    pub static CALLS: AtomicUsize = AtomicUsize::new(0);
    /// Batch size of the last call.
    pub static LAST_BATCH: AtomicUsize = AtomicUsize::new(0);

    /// The extension.
    #[extend(ScaleQuant, addon = "test_scale_upsert_override")]
    pub struct QuantMarked {
        #[field(boolean)]
        pub marked: bool,
        /// A readonly precomputed field: always computed, refused in an upsert row.
        #[field(float, compute = "_compute_double_quantity", store, precompute)]
        pub double_quantity: f64,
        /// A stored related field (no compute attribute): always computed, refused in a row.
        #[field(char, related = "location_id.name", store)]
        pub location_label: Option<Arc<str>>,
    }

    #[methods(ScaleQuant, addon = "test_scale_upsert_override")]
    impl QuantMarked {
        #[compute(depends = ["quantity"])]
        fn _compute_double_quantity(batch: &mut Batch<'_, ScaleQuant>) -> Result<()> {
            for i in batch.indices() {
                let quantity = batch.get(i, ScaleQuant::QUANTITY)?;
                batch.set(i, QuantMarked::DOUBLE_QUANTITY, quantity * 2.0)?;
            }
            Ok(())
        }

        #[method(override)]
        async fn upsert(
            this: &RecordSet<ScaleQuant>,
            tx: &mut Transaction,
            call: UpsertCall,
            sup: Super,
        ) -> Result<UpsertResult> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            LAST_BATCH.store(call.rows().len(), Ordering::SeqCst);
            let result = sup.call(tx, this.as_records(), (call,)).await?;
            this.as_records()
                .browse_ids(result.distinct_ids())
                .write(tx, RawVals::new().set_cell("marked", Cell::Bool(true)))
                .await?;
            Ok(result)
        }
    }
}
