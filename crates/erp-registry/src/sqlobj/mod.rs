// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon-owned SQL objects, slice B1 (contract
//! §2): raw native declarations ([`decl`]), their validation and admission bounds ([`validate`])
//! and the resolved dependency graph on the [`crate::Registry`] ([`graph`]). Pure: no rendering,
//! no DDL and no database (later slices).

/// `SQL object '<addon>/<key>': <reason>` (contract §2) for `who = (addon, key)`; a bound uses the
/// reason `<limit-name> limit <limit> exceeded`.
macro_rules! err {
    ($who:expr, $($reason:tt)+) => {{
        let (addon, key): (&str, &str) = $who;
        erp_fields::Error::programming(format!(
            "SQL object '{addon}/{key}': {}",
            format_args!($($reason)+)
        ))
    }};
}

pub(crate) mod decl;
pub(crate) mod graph;
pub(crate) mod validate;

use erp_fields::{Error, Result};

/// Admission bounds of the SQL object declarations of one registry (contract §2). They are
/// safety limits, not performance measurements. A host may lower any of them (zero means none
/// allowed, never unlimited) but never raise one above [`SqlObjectLimits::DEFAULT`].
/// Identifier components are always at most [`crate::ident::MAX_IDENTIFIER_LEN`] bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SqlObjectLimits {
    /// Bytes of one function body.
    pub body_bytes: usize,
    /// Direct dependencies of one object.
    pub dependencies: usize,
    /// Arguments of one function.
    pub arguments: usize,
    /// Bytes of one condition message.
    pub condition_message_bytes: usize,
    /// Objects of one addon.
    pub addon_objects: usize,
    /// Body and metadata bytes of one addon.
    pub addon_bytes: usize,
    /// Objects of one registry.
    pub registry_objects: usize,
    /// Body and metadata bytes of one registry.
    pub registry_bytes: usize,
}

impl SqlObjectLimits {
    /// The contract's bounds: 256 KiB body, 256 dependencies, 32 arguments, 4096-byte condition
    /// message, 4096 objects and 16 MiB per addon, 65536 objects and 64 MiB per registry.
    pub const DEFAULT: Self = Self {
        body_bytes: 256 * 1024,
        dependencies: 256,
        arguments: 32,
        condition_message_bytes: 4096,
        addon_objects: 4096,
        addon_bytes: 16 * 1024 * 1024,
        registry_objects: 65_536,
        registry_bytes: 64 * 1024 * 1024,
    };

    /// `(name, value, default)` of every bound; the name is the one errors use.
    fn entries(&self) -> [(&'static str, usize, usize); 8] {
        let d = Self::DEFAULT;
        [
            ("body bytes", self.body_bytes, d.body_bytes),
            ("dependencies", self.dependencies, d.dependencies),
            ("arguments", self.arguments, d.arguments),
            (
                "condition message bytes",
                self.condition_message_bytes,
                d.condition_message_bytes,
            ),
            ("addon objects", self.addon_objects, d.addon_objects),
            ("addon bytes", self.addon_bytes, d.addon_bytes),
            (
                "registry objects",
                self.registry_objects,
                d.registry_objects,
            ),
            ("registry bytes", self.registry_bytes, d.registry_bytes),
        ]
    }

    /// Refuse a bound raised above its default.
    pub(crate) fn check_not_raised(&self) -> Result<()> {
        for (name, value, default) in self.entries() {
            if value > default {
                return Err(Error::programming(format!(
                    "SQL object {name} limit {value} is above the default {default}; a host may \
                     only lower it"
                )));
            }
        }
        Ok(())
    }
}

impl Default for SqlObjectLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Refuse `value` above `limit`: `SQL object '<addon>/<key>': <name> limit <limit> exceeded`.
pub(crate) fn within(who: validate::Who<'_>, name: &str, value: usize, limit: usize) -> Result<()> {
    if value > limit {
        return Err(err!(who, "{name} limit {limit} exceeded"));
    }
    Ok(())
}
