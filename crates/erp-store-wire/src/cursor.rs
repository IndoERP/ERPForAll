//! The bus frame cursor `(xid, id)` (4).

use crate::error::WireError;
use crate::xid::{Xid8, parse_decimal};

/// The position of a bus frame: the emitting transaction's xid and the event id.
///
/// The derived ordering is numeric on `xid`, then on `id`; it is never string order
/// (`"9" < "10"`). Both parts are decimal strings on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BusCursor {
    xid: Xid8,
    id: u64,
}

impl BusCursor {
    /// Build a cursor. The event id is a positive `bigint` (1..=2^63-1).
    ///
    /// # Errors
    /// [`WireError::BadDescriptor`] (path `cursor.id`) for an id outside that range.
    pub fn new(xid: Xid8, id: u64) -> Result<Self, WireError> {
        if id == 0 || i64::try_from(id).is_err() {
            return Err(WireError::BadDescriptor {
                path: "cursor.id".to_owned(),
                reason: "event id is outside 1..=2^63-1".to_owned(),
            });
        }
        Ok(Self { xid, id })
    }

    /// Build from the wire strings.
    ///
    /// # Errors
    /// [`WireError::BadDescriptor`] with path `cursor.xid` or `cursor.id`.
    pub fn from_wire_strings(xid: &str, id: &str) -> Result<Self, WireError> {
        let xid = Xid8::parse_at(xid, "cursor.xid")?;
        let id = parse_decimal(id).map_err(|reason| WireError::BadDescriptor {
            path: "cursor.id".to_owned(),
            reason: reason.to_owned(),
        })?;
        Self::new(xid, id)
    }

    /// The emitting transaction's xid.
    #[must_use]
    pub fn xid(&self) -> Xid8 {
        self.xid
    }

    /// The event id.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }
}
