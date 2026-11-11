//! Named resource limits for untrusted wire input (engineering standards §4).
//!
//! Every limit has a compiled-in maximum that is also the default. A host may lower a limit
//! with [`WireLimits::lowered`]; zero is refused and a value above the maximum is refused, so no
//! configuration can switch a bound off or widen it.

use crate::error::WireError;

/// One bounded quantity of a payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Limit {
    /// Size of the whole document in bytes.
    Bytes,
    /// Nesting depth of JSON arrays and objects.
    Depth,
    /// Models in one payload or schema.
    Models,
    /// Record entries in one payload, over all models.
    Records,
    /// Ids in one x2many list.
    IdsPerList,
    /// Length of the in-progress list of a snapshot descriptor.
    Xip,
    /// Size of one string value in bytes.
    StringBytes,
    /// Length of a schema generation token in bytes.
    SchemaTokenBytes,
    /// Fields in one schema, over all models.
    SchemaFields,
}

impl Limit {
    /// Every limit, in declaration order.
    pub const ALL: [Limit; 9] = [
        Limit::Bytes,
        Limit::Depth,
        Limit::Models,
        Limit::Records,
        Limit::IdsPerList,
        Limit::Xip,
        Limit::StringBytes,
        Limit::SchemaTokenBytes,
        Limit::SchemaFields,
    ];

    /// The stable name used in `limit_exceeded` errors.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Limit::Bytes => "bytes",
            Limit::Depth => "depth",
            Limit::Models => "models",
            Limit::Records => "records",
            Limit::IdsPerList => "ids_per_list",
            Limit::Xip => "xip",
            Limit::StringBytes => "string_bytes",
            Limit::SchemaTokenBytes => "schema_token_bytes",
            Limit::SchemaFields => "schema_fields",
        }
    }

    /// The compiled-in maximum, which is also the default value.
    #[must_use]
    pub const fn maximum(self) -> u64 {
        match self {
            Limit::Bytes => 8 * 1024 * 1024,
            Limit::Depth => 16,
            Limit::Models => 256,
            Limit::Records => 50_000,
            Limit::IdsPerList => 50_000,
            Limit::Xip => 65_536,
            Limit::StringBytes => 1024 * 1024,
            Limit::SchemaTokenBytes => 128,
            Limit::SchemaFields => 16_384,
        }
    }

    fn index(self) -> usize {
        match self {
            Limit::Bytes => 0,
            Limit::Depth => 1,
            Limit::Models => 2,
            Limit::Records => 3,
            Limit::IdsPerList => 4,
            Limit::Xip => 5,
            Limit::StringBytes => 6,
            Limit::SchemaTokenBytes => 7,
            Limit::SchemaFields => 8,
        }
    }
}

/// The limits in force for one encoder, decoder or descriptor constructor.
///
/// ```
/// use erp_store_wire::{Limit, WireLimits};
///
/// let limits = WireLimits::default().lowered(Limit::Records, 10)?;
/// assert_eq!(limits.get(Limit::Records), 10);
/// assert!(WireLimits::default().lowered(Limit::Records, 0).is_err());
/// # Ok::<(), erp_store_wire::WireError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireLimits {
    values: [u64; 9],
}

impl Default for WireLimits {
    fn default() -> Self {
        let mut values = [0; 9];
        for limit in Limit::ALL {
            values[limit.index()] = limit.maximum();
        }
        Self { values }
    }
}

impl WireLimits {
    /// The value in force for `limit`.
    #[must_use]
    pub fn get(&self, limit: Limit) -> u64 {
        self.values[limit.index()]
    }

    /// A copy with `limit` set to `value`.
    ///
    /// # Errors
    /// [`WireError::BadLimit`] when `value` is zero or above [`Limit::maximum`].
    pub fn lowered(mut self, limit: Limit, value: u64) -> Result<Self, WireError> {
        if value == 0 {
            return Err(WireError::BadLimit {
                limit: limit.name(),
                reason: "zero is not a limit".to_owned(),
            });
        }
        if value > limit.maximum() {
            return Err(WireError::BadLimit {
                limit: limit.name(),
                reason: format!(
                    "{value} is above the compiled-in maximum {}",
                    limit.maximum()
                ),
            });
        }
        self.values[limit.index()] = value;
        Ok(self)
    }

    /// The refusal for an input of `size` against `limit`, or `Ok` when it fits.
    pub(crate) fn check(&self, limit: Limit, size: usize, path: &str) -> Result<(), WireError> {
        let maximum = self.get(limit);
        if u64::try_from(size).map_or(true, |size| size > maximum) {
            return Err(WireError::LimitExceeded {
                limit: limit.name(),
                path: path.to_owned(),
                maximum,
            });
        }
        Ok(())
    }
}
