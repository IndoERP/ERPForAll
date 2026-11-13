// SPDX-License-Identifier: MIT OR Apache-2.0
//! Portable bounded barcode engine. Snapshot and output v1 are defined.
mod encoding;
mod pattern;
mod uri;

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// A classified error whose stable message is shared by native and browser callers.
#[derive(Debug)]
#[non_exhaustive]
pub struct BarcodeError {
    kind: &'static str,
    message: &'static str,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}
impl BarcodeError {
    pub(crate) const fn new(kind: &'static str, message: &'static str) -> Self {
        Self {
            kind,
            message,
            source: None,
        }
    }
    pub(crate) fn with_source(
        mut self,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        self.source = Some(Box::new(source));
        self
    }
    /// Stable classification: configuration, pattern, limit, uri or unsupported.
    pub const fn kind(&self) -> &'static str {
        self.kind
    }
}
impl fmt::Display for BarcodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for BarcodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}
type Result<T> = std::result::Result<T, BarcodeError>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    conversion: Conversion,
    rules: Vec<Rule>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Conversion {
    None,
    Ean2upc,
    Upc2ean,
    Always,
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Encoding {
    Any,
    Ean8,
    Ean13,
    Upca,
}
impl Encoding {
    const fn key(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Ean8 => "ean8",
            Self::Ean13 => "ean13",
            Self::Upca => "upca",
        }
    }
    const fn width(self) -> Option<usize> {
        match self {
            Self::Any => None,
            Self::Ean8 => Some(8),
            Self::Ean13 => Some(13),
            Self::Upca => Some(12),
        }
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    id: i64,
    sequence: i32,
    encoding: Encoding,
    kind: String,
    pattern: String,
    alias: String,
}
#[derive(Debug)]
struct CompiledRule {
    rule: Rule,
    pattern: pattern::Pattern,
}

#[derive(Serialize)]
struct Item {
    encoding: String,
    #[serde(rename = "type")]
    kind: String,
    code: String,
    base_code: String,
    value: String,
}
#[derive(Serialize)]
struct Outcome {
    version: u32,
    status: &'static str,
    original_code: String,
    code: String,
    items: Vec<Item>,
}

/// Immutable validated rules. No database, filesystem or network capability is used.
#[derive(Debug)]
#[non_exhaustive]
pub struct Nomenclature {
    conversion: Conversion,
    rules: Vec<CompiledRule>,
}
impl Nomenclature {
    /// Compile a strict v1 snapshot. All allocation-bearing inputs have fixed ceilings.
    pub fn from_json(source: &str) -> Result<Self> {
        if source.len() > 1024 * 1024 {
            return Err(BarcodeError::new(
                "limit",
                "Snapshot exceeds 1048576 bytes.",
            ));
        }
        let mut snapshot: Snapshot = serde_json::from_str(source).map_err(|error| {
            BarcodeError::new("configuration", "Invalid barcode snapshot JSON.").with_source(error)
        })?;
        if snapshot.version != 1 {
            return Err(BarcodeError::new(
                "configuration",
                "Unsupported barcode snapshot version.",
            ));
        }
        if snapshot.rules.len() > 256 {
            return Err(BarcodeError::new(
                "limit",
                "Nomenclature exceeds 256 rules.",
            ));
        }
        let mut ids = BTreeSet::new();
        let mut source_bytes = 0usize;
        for rule in &snapshot.rules {
            if rule.id <= 0 || !ids.insert(rule.id) {
                return Err(BarcodeError::new(
                    "configuration",
                    "Rule IDs must be positive and unique.",
                ));
            }
            if rule.kind.is_empty()
                || rule.kind.len() > 64
                || !rule
                    .kind
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_')
            {
                return Err(BarcodeError::new(
                    "configuration",
                    "Invalid barcode rule type key.",
                ));
            }
            if rule.alias.len() > 4096 {
                return Err(BarcodeError::new("limit", "Alias exceeds 4096 bytes."));
            }
            source_bytes = source_bytes
                .checked_add(rule.pattern.len())
                .ok_or_else(|| BarcodeError::new("limit", "Rule patterns exceed 8192 bytes."))?;
            if source_bytes > 8192 {
                return Err(BarcodeError::new(
                    "limit",
                    "Rule patterns exceed 8192 bytes.",
                ));
            }
        }
        snapshot.rules.sort_by_key(|r| (r.sequence, r.id));
        let compiled_budget = (8 * 1024 * 1024 / snapshot.rules.len().max(1)).min(1024 * 1024);
        let rules = snapshot
            .rules
            .into_iter()
            .map(|rule| {
                let pattern = pattern::Pattern::compile(&rule.pattern, compiled_budget)?;
                Ok(CompiledRule { rule, pattern })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            conversion: snapshot.conversion,
            rules,
        })
    }
    /// Parse a code and return the common v1 JSON result, preserving exact decimal strings.
    pub fn parse_json(&self, original: &str) -> Result<String> {
        if original.len() > 4096 {
            return Err(BarcodeError::new("limit", "Barcode exceeds 4096 bytes."));
        }
        let mut out = Outcome {
            version: 1,
            status: "no_match",
            original_code: original.into(),
            code: original.into(),
            items: Vec::new(),
        };
        if original.starts_with("urn:") {
            out.items = uri::parse(original)?;
            out.status = "uri";
        } else {
            for compiled in &self.rules {
                let rule = &compiled.rule;
                let candidate = match (&self.conversion, rule.encoding) {
                    (Conversion::Upc2ean | Conversion::Always, Encoding::Ean13)
                        if encoding::valid(&out.code, Encoding::Upca) =>
                    {
                        format!("0{}", out.code)
                    }
                    (Conversion::Ean2upc | Conversion::Always, Encoding::Upca)
                        if out.code.starts_with('0')
                            && encoding::valid(&out.code, Encoding::Ean13) =>
                    {
                        out.code[1..].to_owned()
                    }
                    _ => out.code.clone(), // conversion is conditional; otherwise retain input.
                };
                if !encoding::valid(&candidate, rule.encoding) {
                    continue;
                }
                if let Some((mut base_code, value)) = compiled.pattern.apply(&candidate)? {
                    if rule.kind == "alias" {
                        out.code.clone_from(&rule.alias);
                        continue;
                    }
                    if rule.encoding.width().is_some() {
                        base_code = encoding::repair(&base_code)?;
                    }
                    out.status = "matched";
                    out.code.clone_from(&candidate);
                    out.items.push(Item {
                        encoding: rule.encoding.key().into(),
                        kind: rule.kind.clone(),
                        code: candidate,
                        base_code,
                        value,
                    });
                    break;
                }
            }
        }
        serde_json::to_string(&out)
            .map_err(|_| BarcodeError::new("configuration", "Cannot serialize barcode result."))
    }
}
