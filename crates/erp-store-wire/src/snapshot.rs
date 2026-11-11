//! The snapshot descriptor of one transaction (4,/014).

use crate::error::WireError;
use crate::limits::{Limit, WireLimits};
use crate::xid::Xid8;

/// `{xmin, xmax, xip, own}`: what a transaction's snapshot knew when it answered or emitted.
///
/// Invariants, enforced by every constructor: `xmin <= xmax`; `xip` is strictly ascending and
/// inside `[xmin, xmax)`; `own` is absent or at least `xmax` (A1: the snapshot is fixed before the xid is assigned); the list is never truncated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotDescriptor {
    xmin: Xid8,
    xmax: Xid8,
    xip: Vec<Xid8>,
    own: Option<Xid8>,
}

impl SnapshotDescriptor {
    /// Build a descriptor under the default limits.
    ///
    /// # Errors
    /// [`WireError::BadDescriptor`] for a broken invariant, [`WireError::LimitExceeded`] when
    /// `xip` is longer than the `xip` limit.
    pub fn new(
        xmin: Xid8,
        xmax: Xid8,
        xip: Vec<Xid8>,
        own: Option<Xid8>,
    ) -> Result<Self, WireError> {
        Self::new_with_limits(xmin, xmax, xip, own, &WireLimits::default())
    }

    /// [`new`](Self::new) under the host's limits.
    ///
    /// # Errors
    /// As [`new`](Self::new).
    pub fn new_with_limits(
        xmin: Xid8,
        xmax: Xid8,
        xip: Vec<Xid8>,
        own: Option<Xid8>,
        limits: &WireLimits,
    ) -> Result<Self, WireError> {
        limits.check(Limit::Xip, xip.len(), "asof.xip")?;
        if xmin > xmax {
            return Err(bad("asof", "xmin is above xmax"));
        }
        for (i, entry) in xip.iter().enumerate() {
            if *entry < xmin || *entry >= xmax {
                return Err(bad(
                    &format!("asof.xip[{i}]"),
                    "xip entry is outside [xmin, xmax)",
                ));
            }
            if i > 0 && xip[i - 1] >= *entry {
                return Err(bad(
                    &format!("asof.xip[{i}]"),
                    "xip is not strictly ascending",
                ));
            }
        }
        if let Some(own) = own
            && xip.binary_search(&own).is_ok()
        {
            return Err(bad("asof.own", "own is in xip"));
        }
        // A1: the snapshot is fixed before the transaction's xid is assigned, so own is never below
        // xmax. C(D) and n(D) agree with each other only under this invariant.
        if let Some(own) = own
            && own < xmax
        {
            return Err(bad("asof.own", "own is below xmax"));
        }
        Ok(Self {
            xmin,
            xmax,
            xip,
            own,
        })
    }

    /// Build from the wire strings (`xmin`, `xmax`, `xip[i]`, `own`), each a canonical xid8.
    ///
    /// # Errors
    /// [`WireError::BadDescriptor`] with the path of the offending member (`asof.xmin`,
    /// `asof.xip[2]`, ...), or the errors of [`new_with_limits`](Self::new_with_limits).
    pub fn from_wire_strings(
        xmin: &str,
        xmax: &str,
        xip: &[&str],
        own: Option<&str>,
        limits: &WireLimits,
    ) -> Result<Self, WireError> {
        // The length is checked before any entry is parsed, so an oversize list costs nothing.
        limits.check(Limit::Xip, xip.len(), "asof.xip")?;
        let xmin = Xid8::parse_at(xmin, "asof.xmin")?;
        let xmax = Xid8::parse_at(xmax, "asof.xmax")?;
        let mut list = Vec::with_capacity(xip.len());
        for (i, text) in xip.iter().enumerate() {
            list.push(Xid8::parse_at(text, &format!("asof.xip[{i}]"))?);
        }
        let own = own
            .map(|text| Xid8::parse_at(text, "asof.own"))
            .transpose()?;
        Self::new_with_limits(xmin, xmax, list, own, limits)
    }

    /// Parse the text of PostgreSQL's `pg_current_snapshot()`, `xmin:xmax:xip,xip,...`, with the
    /// answering transaction's own xid (from `pg_current_xact_id_if_assigned()`).
    ///
    /// # Errors
    /// [`WireError::BadDescriptor`] (path `snapshot` for the overall shape) or the errors of
    /// [`from_wire_strings`](Self::from_wire_strings).
    pub fn parse_pg_snapshot(text: &str, own: Option<Xid8>) -> Result<Self, WireError> {
        let shape = || bad("snapshot", "expected xmin:xmax:xip,...");
        let mut parts = text.split(':');
        let (Some(xmin), Some(xmax), Some(list), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(shape());
        };
        let xip: Vec<&str> = if list.is_empty() {
            Vec::new()
        } else {
            list.split(',').collect()
        };
        let own = own.map(|v| v.get().to_string());
        Self::from_wire_strings(xmin, xmax, &xip, own.as_deref(), &WireLimits::default())
    }

    /// The lowest transaction id still active when the snapshot was taken.
    #[must_use]
    pub fn xmin(&self) -> Xid8 {
        self.xmin
    }

    /// One past the highest completed transaction id.
    #[must_use]
    pub fn xmax(&self) -> Xid8 {
        self.xmax
    }

    /// The transactions in `[xmin, xmax)` that had not completed, ascending.
    #[must_use]
    pub fn xip(&self) -> &[Xid8] {
        &self.xip
    }

    /// The answering transaction's own xid when it had one.
    #[must_use]
    pub fn own(&self) -> Option<Xid8> {
        self.own
    }

    /// The reflection predicate.5: is a transaction known to this snapshot?
    #[must_use]
    pub fn reflects(&self, xid: Xid8) -> bool {
        xid < self.xmin
            || (xid < self.xmax && self.xip.binary_search(&xid).is_err())
            || self.own == Some(xid)
    }

    /// The completed count `xmax - |xip|`, the order key between two answers.
    #[must_use]
    pub fn completed_count(&self) -> u64 {
        // xip is strictly ascending inside [xmin, xmax), so its length is at most xmax and the
        // subtraction cannot underflow (saturating only documents that); the length fits u64.
        self.xmax.get().saturating_sub(self.xip.len() as u64)
    }
}

fn bad(path: &str, reason: &str) -> WireError {
    WireError::BadDescriptor {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

impl SnapshotDescriptor {
    /// Append the canonical JSON object (`own`, `xip`, `xmax`, `xmin` in ascending key order).
    pub(crate) fn write_json(&self, out: &mut String) {
        out.push_str("{\"own\":");
        match self.own {
            Some(own) => out.push_str(&format!("\"{own}\"")),
            None => out.push_str("null"),
        }
        out.push_str(",\"xip\":[");
        for (i, xid) in self.xip.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"{xid}\""));
        }
        out.push_str(&format!(
            "],\"xmax\":\"{}\",\"xmin\":\"{}\"}}",
            self.xmax, self.xmin
        ));
    }
}
