//! Fixture addon `test_wasm_refs`: names the fields of
//! `test.wasm.dual` with `model_ref!` (three of them declared by `test_wasm_dual_a`, one by
//! `test_wasm_dual_b`), creates and writes records with `Vals`, follows a many2one with
//! `related`, and overrides `create` of the model it extends. The source is the same for the
//! native build and the `wasm32-wasip2` component; no `cfg` appears in it.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_wasm_refs", depends = ["test_wasm_dual_a"]);

// the leaf crate exports the component; natively this expands to nothing
erp_sdk::export_component!();

erp_sdk::model_ref!(
    /// `test.wasm.dual`: `name`, `qty` and `double_qty` belong to `test_wasm_dual_a`, `note` and
    /// `loan_date` to `test_wasm_dual_b`.
    pub DualRef = "test.wasm.dual" {
        name: Option<Arc<str>>,
        qty: i32,
        double_qty: i32,
        note: Option<Arc<str>>,
        loan_date: Option<Date>,
    }
);

/// What this layer adds to the name of every dual created, so a test can see that a `create` made
/// by a guest ran through the override chain.
pub const CREATE_MARK: &str = "+refs";

/// This addon's own field on `test.wasm.dual`.
#[extend(DualRef)]
pub struct DualRefOps {
    /// A free tag.
    #[field(char)]
    pub refs_tag: Option<Arc<str>>,
}

/// `test.wasm.refs.line`: points at a dual.
#[model(
    name = "test.wasm.refs.line",
    description = "Typed reference fixture line"
)]
pub struct Line {
    /// The dual it belongs to.
    #[field(many2one, comodel = "test.wasm.dual")]
    pub dual_id: Many2one<DualRef>,
    /// A tag.
    #[field(char)]
    pub tag: Option<Arc<str>>,
}

#[methods(DualRef)]
impl DualRefOps {
    /// Create one dual per name through typed values and answer their ids.
    #[method(wire)]
    async fn spawn(
        this: &RecordSet<DualRef>,
        tx: &mut Transaction,
        names: Vec<String>,
    ) -> Result<Vec<i64>> {
        let duals = this.env().model::<DualRef>()?;
        let vals = names
            .iter()
            .map(|n| {
                duals
                    .vals()
                    .set(DualRef::NAME, n.as_str())
                    .set(DualRef::QTY, 7)
            })
            .collect();
        let made = duals.create(tx, vals).await?;
        Ok(made.rec_ids().iter().map(|id| id.get()).collect())
    }

    /// Write two fields of every record at once.
    #[method(wire)]
    async fn rename(
        this: &RecordSet<DualRef>,
        tx: &mut Transaction,
        name: String,
        qty: i32,
    ) -> Result<()> {
        let vals = this
            .vals()
            .set(DualRef::NAME, name.as_str())
            .set(DualRef::QTY, qty);
        this.write(tx, vals).await
    }

    /// Set the note (a field another addon extended the model with) and read it back.
    #[method(wire)]
    async fn annotate(
        this: &RecordSet<DualRef>,
        tx: &mut Transaction,
        note: String,
    ) -> Result<String> {
        let one = this.ensure_one()?;
        one.set(tx, DualRef::NOTE, note.as_str()).await?;
        Ok(one
            .get(tx, DualRef::NOTE)
            .await?
            .map(|n| n.to_string())
            .unwrap_or_default())
    }

    /// Write a date (as a day count) in the same `write` as a text, and read the date back.
    #[method(wire)]
    async fn stamp(this: &RecordSet<DualRef>, tx: &mut Transaction, day: i32) -> Result<i32> {
        let one = this.ensure_one()?;
        let vals = one
            .vals()
            .set(DualRef::LOAN_DATE, Date::from_days(day))
            .set(DualRef::NOTE, "stamped");
        one.write(tx, vals).await?;
        Ok(one
            .get(tx, DualRef::LOAN_DATE)
            .await?
            .map_or(-1, Date::days))
    }

    /// Create a line pointing at this dual, in another model, and answer its id.
    #[method(wire)]
    async fn add_line(this: &RecordSet<DualRef>, tx: &mut Transaction, tag: String) -> Result<i64> {
        let dual = this.ensure_one()?;
        let lines = this.env().model::<Line>()?;
        let vals = lines
            .vals()
            .set(Line::DUAL_ID, dual.rec_ids()[0])
            .set(Line::TAG, tag.as_str());
        let made = lines.create(tx, vec![vals]).await?;
        Ok(made.rec_ids()[0].get())
    }

    /// Mark the name of every new dual, then create through the layers below.
    #[method(override)]
    async fn create(
        this: &RecordSet<DualRef>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        for vals in &mut vals_list {
            if let Some(Cell::Str(name)) = vals.get("name").cloned() {
                vals.insert(
                    "name".into(),
                    Cell::Str(format!("{name}{CREATE_MARK}").into()),
                );
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }
}

#[methods(Line)]
impl Line {
    /// The name of the dual each line points at, one per line (a many2one followed to a recordset
    /// and read).
    #[method(wire)]
    async fn dual_names(this: &RecordSet<Line>, tx: &mut Transaction) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for line in this.iter() {
            let dual = line.related(tx, Line::DUAL_ID).await?;
            names.push(
                dual.get(tx, DualRef::NAME)
                    .await?
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
            );
        }
        Ok(names)
    }

    /// The ids of the duals the lines point at, each once (the related set of several lines).
    #[method(wire)]
    async fn dual_ids(this: &RecordSet<Line>, tx: &mut Transaction) -> Result<Vec<i64>> {
        let duals = this.related(tx, Line::DUAL_ID).await?;
        Ok(duals.rec_ids().iter().map(|id| id.get()).collect())
    }

    /// How many records an empty recordset of another model holds.
    #[method(wire)]
    async fn empty_duals(this: &RecordSet<Line>, tx: &mut Transaction) -> Result<usize> {
        let _ = tx;
        Ok(this.env().model::<DualRef>()?.len())
    }
}
