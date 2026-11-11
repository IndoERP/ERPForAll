use crate::error::{Error, Result};
use crate::manifest::ModuleName;
use serde::Serialize;
use std::fmt;

pub const ODOO_SERIES_PREFIX: &str = "20.0.";
pub const ODOO_SERIES: &str = "20.0";
pub const ERP_SERIES: &str = "1.0";

/// The semver form of a version. Defined next to the native migration declarations
/// (`erp_data::migration`), whose functions receive it; re-exported here under its old path.
pub use erp_data::migration::SemverVersion;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
pub enum ModuleVersion {
    Compatible { raw: String, semver: SemverVersion },
    Incompatible { raw: String, reason: String },
}

impl fmt::Display for ModuleVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compatible { semver, .. } => write!(f, "{semver}"),
            Self::Incompatible { raw, .. } => write!(f, "{raw}"),
        }
    }
}

impl ModuleVersion {
    /// Default version for new modules and.
    pub fn default_version() -> Self {
        Self::compatible("1.0.0", 1, 0, 0)
    }

    /// Construct a compatible version variant.
    pub fn compatible(raw: impl Into<String>, major: u32, minor: u32, patch: u32) -> Self {
        Self::Compatible {
            raw: raw.into(),
            semver: SemverVersion::new(major, minor, patch),
        }
    }

    /// Construct an incompatible version variant.
    pub fn incompatible(raw: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Incompatible {
            raw: raw.into(),
            reason: reason.into(),
        }
    }

    /// Returns the raw version string as declared.
    pub fn raw(&self) -> &str {
        match self {
            Self::Compatible { raw, .. } => raw,
            Self::Incompatible { raw, .. } => raw,
        }
    }

    /// Returns the parsed semver version if compatible.
    pub fn semver(&self) -> Option<&SemverVersion> {
        match self {
            Self::Compatible { semver, .. } => Some(semver),
            Self::Incompatible { .. } => None,
        }
    }

    /// Returns true if this version is compatible with ERPForAll series.
    pub fn is_compatible(&self) -> bool {
        matches!(self, Self::Compatible { .. })
    }

    /// Parse and adapt a manifest version string according to,.
    ///
    /// Rules:
    /// 1. A version must have 2 to 5 dot-separated integer components. Fewer or more components,
    ///    non-numeric characters, signs, or leading zeros (except single '0') raise an error.
    /// 2. The bare series "20.0" is valid (D4 accepted) and maps to module version "0.0.0".
    /// 3. If version starts with "20.0." and map remainder:
    ///    - 1 part `x` -> `x.0.0`
    ///    - 2 parts `x.y` -> `x.y.0`
    ///    - 3 parts `x.y.z` -> `x.y.z`
    /// 4. If version has 2 parts `x.y` -> adapted to `20.0.x.y` -> `x.y.0`.
    /// 5. If version has 3 parts `x.y.z` -> adapted to `20.0.x.y.z` -> `x.y.z`.
    /// 6. Versions with 4 or 5 parts not starting with "20.0." (e.g. `19.0.1.0` or `1.2.3.4`) are
    ///    incompatible series: `ModuleVersion::Incompatible`.
    pub fn parse(module_name: &ModuleName, raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let parts: Vec<&str> = raw.split('.').collect();

        // Must have 2 to 5 parts
        if parts.len() < 2 || parts.len() > 5 {
            return Err(Error::InvalidVersion {
                name: module_name.clone(),
                version: raw.to_string(),
                message: format!(
                    "version must have 2 to 5 dot-separated integer components, got {}",
                    parts.len()
                ),
            });
        }

        // All parts must be non-negative integers (ASCII digits only, no sign, no leading zeros)
        let mut int_parts = Vec::with_capacity(parts.len());
        for p in &parts {
            if p.is_empty() {
                return Err(Error::InvalidVersion {
                    name: module_name.clone(),
                    version: raw.to_string(),
                    message: "version component cannot be empty".to_string(),
                });
            }
            if !p.chars().all(|c| c.is_ascii_digit()) {
                return Err(Error::InvalidVersion {
                    name: module_name.clone(),
                    version: raw.to_string(),
                    message: format!("component '{p}' contains non-ASCII-digit characters"),
                });
            }
            if p.len() > 1 && p.starts_with('0') {
                return Err(Error::InvalidVersion {
                    name: module_name.clone(),
                    version: raw.to_string(),
                    message: format!("component '{p}' has invalid leading zero"),
                });
            }
            match p.parse::<u32>() {
                Ok(n) => int_parts.push(n),
                Err(_) => {
                    return Err(Error::InvalidVersion {
                        name: module_name.clone(),
                        version: raw.to_string(),
                        message: format!("component '{p}' overflows u32"),
                    });
                }
            }
        }

        // Case A: bare series "20.0" -> valid, maps to module version 0.0.0
        if raw == ODOO_SERIES {
            return Ok(Self::compatible(raw, 0, 0, 0));
        }

        if raw.starts_with(ODOO_SERIES_PREFIX) {
            let remainder = &int_parts[2..];
            let sv = match remainder.len() {
                1 => SemverVersion::new(remainder[0], 0, 0),
                2 => SemverVersion::new(remainder[0], remainder[1], 0),
                3 => SemverVersion::new(remainder[0], remainder[1], remainder[2]),
                _ => {
                    return Ok(Self::incompatible(
                        raw,
                        format!(
                            "series remainder with {} parts is unsupported",
                            remainder.len()
                        ),
                    ));
                }
            };
            return Ok(Self::Compatible {
                raw: raw.to_string(),
                semver: sv,
            });
        }

        // Case C: 2 parts `x.y` -> adapted to `20.0.x.y` -> `x.y.0`
        if int_parts.len() == 2 {
            return Ok(Self::compatible(raw, int_parts[0], int_parts[1], 0));
        }

        // Case D: 3 parts `x.y.z` -> adapted to `20.0.x.y.z` -> `x.y.z`
        if int_parts.len() == 3 {
            return Ok(Self::compatible(
                raw,
                int_parts[0],
                int_parts[1],
                int_parts[2],
            ));
        }

        // Case E: 4 or 5 parts not starting with 20.0. (e.g. 19.0.1.0 or 1.2.3.4) -> incompatible
        Ok(Self::incompatible(
            raw,
            format!("incompatible version series: expected '{ODOO_SERIES}' series"),
        ))
    }
}
