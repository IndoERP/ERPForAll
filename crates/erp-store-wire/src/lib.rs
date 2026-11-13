//! The messaging store wire contract.
//!
//! Records grouped by model, field operations, a delete marker, a wire major version, a schema
//! generation and a PostgreSQL snapshot descriptor, plus the validated types that carry them.
//! The codec is generic: it knows models and fields only through a [`StoreSchema`] it is given
//! and contains no `mail` code.
//!
//! Layer L1 (amendment 2026-11-06): no internal dependencies, no I/O, no clock, no
//! threads, builds for `wasm32-wasip2`. Everything here is stateless; any node encodes the same
//! bytes for the same input.

mod cursor;
mod decode;
pub mod doc;
mod error;
mod json;
mod limits;
mod payload;
mod schema;
mod snapshot;
mod value;
mod version;
mod xid;

pub use cursor::BusCursor;
pub use decode::{DecodedField, DecodedPayload, DecodedRecord, Decoder, Outcome};
pub use error::WireError;
pub use limits::{Limit, WireLimits};
pub use payload::{AnswerBuilder, FieldValue, FrameBuilder, RecordRef};
pub use schema::{
    FieldKind, FieldName, FieldSchema, FieldSpec, ModelName, ModelSchema, SchemaBuilder,
    StoreSchema, SyncClass,
};
pub use snapshot::SnapshotDescriptor;
pub use version::{SchemaGeneration, WireMajor, accept_client_major};
pub use xid::Xid8;
