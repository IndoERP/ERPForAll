//! Addon `stock` for ERPForAll.
//!
//! Following boundary §4.1 and Decision N8, `stock` manages warehouse topology,
//! quants, moves, pickings, and locations, but carries no valuation tables or fields.
//! Stock valuation belongs to the `stock_account` bridge addon.
//!
//! Installed so far: the topology tables (locations with their policy rows,
//! warehouses, operation types, routes, rules, storage categories, package types, removal
//! strategies and the topology version row), without behaviour beyond the stored complete name
//! of a location and the policy row created with each location; the tracking tables of
//! (lots, lot-name scope rows, packages, quants, serial positions), schema only;
//! the operations tables (references, transfers with their move counter
//! vector, moves with their product-unit conversion inputs), schema only; the move line tables of
//! (move lines, serial rows and correction rows on the lines' shared id
//! sequence), schema only;
//! and the replenishment configuration tables (orderpoints, putaway rules), schema only.

erp_sdk::addon!(
    name = "stock",
    exact_only = true,
    depends = ["product", "barcodes_gs1_nomenclature"]
);

mod barcode;
pub mod error;
pub mod location;
pub mod lot;
pub mod orderpoint;
pub mod package;
pub mod picking;
pub mod putaway;
pub mod quant;
pub mod reference;
pub mod route;
pub mod stock_move;
pub mod stock_move_line;
pub mod storage;
pub mod warehouse;

pub use error::StockError;
pub use location::{ProductRemoval, StockLocation, StockLocationPolicy};
pub use lot::{StockLot, StockLotNameScope};
pub use orderpoint::StockWarehouseOrderpoint;
pub use package::StockPackage;
pub use picking::StockPicking;
pub use putaway::StockPutawayRule;
pub use quant::{StockQuant, StockSerialPosition};
pub use reference::StockReference;
pub use route::{StockRoute, StockRule, StockTopologyVersion};
pub use stock_move::StockMove;
pub use stock_move_line::{
    LOGICAL_ID_SEQUENCE, StockMoveLine, StockMoveLineCorrection, StockMoveLineSerial,
};
pub use storage::{StockPackageType, StockStorageCategory, StockStorageCategoryCapacity};
pub use warehouse::{StockPickingType, StockWarehouse};
