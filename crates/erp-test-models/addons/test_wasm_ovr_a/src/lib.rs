//! Fixture addon `test_wasm_ovr_a`: defines `test.wasm.ovr` and overrides `create`, `write` and
//! `unlink` natively, below the WebAssembly layer of `test_wasm_ovr_w`.

use std::sync::Mutex;

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_wasm_ovr_a", depends = ["base"]);

/// What the layers did, in order: `"<layer>:<method>"`. A test clears it first.
pub static TRAIL: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Appends to [`TRAIL`].
pub fn note(entry: &str) {
    if let Ok(mut trail) = TRAIL.lock() {
        trail.push(entry.to_owned());
    }
}

/// `test.wasm.ovr`.
#[model(name = "test.wasm.ovr", description = "WASM override fixture")]
pub struct Ovr {
    /// Title.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Quantity.
    #[field(integer, default = 1)]
    pub qty: i32,
    /// Kept from most readers: a package compute may not copy it.
    #[field(char, groups = "base.group_system")]
    pub secret: Option<Arc<str>>,
    /// Written by the WebAssembly layer, so a test can see its change reached the database.
    #[field(char)]
    pub stamp: Option<Arc<str>>,
}

#[methods(Ovr)]
impl Ovr {
    /// Records that the native bottom layer ran, then the core create.
    #[method(override)]
    async fn create(
        this: &RecordSet<Ovr>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        note("a:create");
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// Records that the native bottom layer ran, then the core write.
    #[method(override)]
    async fn write(
        this: &RecordSet<Ovr>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        note("a:write");
        sup.call(tx, this.as_records(), (vals,)).await
    }

    /// Records that the native bottom layer ran, then the core unlink.
    #[method(override)]
    async fn unlink(this: &RecordSet<Ovr>, tx: &mut Transaction, sup: Super) -> Result<()> {
        note("a:unlink");
        sup.call(tx, this.as_records(), ()).await
    }
}
