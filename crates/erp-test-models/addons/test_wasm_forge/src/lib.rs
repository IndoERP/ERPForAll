//! Fixture addon `test_wasm_forge`: a component whose `create` override can lie about the records
//! it created, whose constraint tries to write, and whose method writes in whatever environment it
//! is called in. The host must refuse each (security review r1 H1, H3).
//! Compiled to a component for `wasm_forge_component.rs`; natively it only declares the same
//! contributions.

use erp_addon_test_wasm_ovr_a::Ovr;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_wasm_forge", depends = ["test_wasm_ovr_a"]);

// the leaf crate exports the component; natively this expands to nothing
erp_sdk::export_component!();

/// A `Records` of exactly `ids`: only a component can build one from ids it did not create.
#[cfg(target_family = "wasm")]
fn forged(ids: &[i64]) -> Result<Records> {
    erp_sdk::serde_json::from_value(erp_sdk::serde_json::json!(ids))
        .map_err(|e| Error::internal(e.to_string()))
}

#[cfg(not(target_family = "wasm"))]
fn forged(_ids: &[i64]) -> Result<Records> {
    Err(Error::programming(
        "a fixture that only runs as a component",
    ))
}

/// The extension of `test.wasm.ovr` by the misbehaving component.
#[extend(Ovr)]
pub struct OvrForge {
    /// A free note.
    #[field(char)]
    pub note: Option<Arc<str>>,
}

#[methods(Ovr)]
impl OvrForge {
    /// Creates through the layers below, then answers what the first record's name asks for:
    /// `forge-drop` no records, `forge-dup` the first record twice, `forge-shift` another record's
    /// id, `forge-skip` creates nothing at all and answers no records (honest). Any other name
    /// answers what was created.
    #[method(override)]
    async fn create(
        this: &RecordSet<Ovr>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let mode = match vals_list.first().and_then(|v| v.get("name").cloned()) {
            Some(Cell::Str(name)) => name.to_string(),
            _ => String::new(),
        };
        if mode == "forge-skip" {
            return forged(&[]);
        }
        if mode == "forge-short" {
            vals_list.pop();
        }
        let made = sup.call(tx, this.as_records(), (vals_list,)).await?;
        let ids: Vec<i64> = made.rec_ids().iter().map(|id| id.get()).collect();
        match (mode.as_str(), ids.first()) {
            ("forge-drop", _) => forged(&[]),
            ("forge-dup", Some(first)) => forged(&[*first, *first]),
            ("forge-shift", Some(first)) => forged(&[*first + 1_000_000]),
            _ => Ok(made),
        }
    }

    /// Reads the quantities (a constraint may), and for 777 tries to write (it may not).
    #[constrains("qty")]
    async fn check_forge(this: &RecordSet<Ovr>, tx: &mut Transaction) -> Result<()> {
        let quantities = this.mapped(tx, Ovr::QTY).await?;
        if quantities.contains(&777) {
            this.set(tx, Ovr::NAME, "written by a constraint").await?;
        }
        Ok(())
    }

    /// Writes the name of the record it is called on, in whatever environment it is called in.
    #[method(wire)]
    async fn touch(this: &RecordSet<Ovr>, tx: &mut Transaction) -> Result<String> {
        this.ensure_one()?.set(tx, Ovr::NAME, "touched").await?;
        Ok("done".to_owned())
    }
}
