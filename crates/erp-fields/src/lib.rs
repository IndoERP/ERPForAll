//! ERPForAll field type system (layer L1; pure, builds for `wasm32-wasip2`).
//!
//! - [`FieldType`] and [`ColumnType`]: what a field is and how it is stored.
//! - [`Cell`]: one value in storage form; [`FieldValue`] maps cells to typed Rust values.
//! - [`json`]: the strict JSON parser and RFC 8785 canonical writer.
//! - [`Value`] and [`convert::to_cell`]: the dynamic layer and its single converter.
//! - [`Error`] / [`ErrorKind`]: the error taxonomy, shared by every crate above.
//! - [`Range`], [`RangeBounds`], [`RangeValue`]: values of `date_range`/`datetime_range` fields
//!   (design §5.2).
//! - [`rows`]: columnar result batches ([`rows::Rows`]) and their handles in [`Value::Row`] /
//!   [`Value::Rows`].

pub mod base64;
mod bigint;
pub use bigint::BigInt;
mod cell;
pub mod convert;
mod error;
pub mod html;
pub mod json;
mod label;
pub mod money;
#[cfg(feature = "pg")]
mod pg;
mod range;
pub mod rows;
mod temporal;
mod timestamp_tz;
mod types;
pub use timestamp_tz::TimestampTz;
mod uuid;
mod xid8;
pub use xid8::Xid8;
mod value;
pub use uuid::Uuid;

pub use cell::{
    Cell, FieldTranslations, FieldValue, IntoFieldValue, RealId, RecId, RefType, ValueFor, kinds,
};
pub use error::{Condition, Error, ErrorKind, Result};
pub use label::default_label;
#[cfg(feature = "pg")]
pub use pg::{RangeSql, StoredRange};
pub use range::{DateRange, DatetimeRange, Range, RangeBounds, RangeValue};
pub use rust_decimal::Decimal;
pub use serde_json;
pub use temporal::{Date, DateTime};
pub use types::{
    ColumnGenerated, ColumnType, Digits, FieldType, GeneratedStorage, IndexKind, M2mRelation,
    OnDelete, ParseSelectionOnDeleteError, SelectionOnDelete, Storage, TextCollation,
};
pub use value::{Value, ValueMap};
