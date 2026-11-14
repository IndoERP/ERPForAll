// SPDX-License-Identifier: MIT OR Apache-2.0
//! Declared technical system parameters: typed, bounded keys of `ir.config_parameter`
//! that an addon owns. Shown in System Parameters only, never on a Settings page.
use super::{SettingGroupDecl, SettingKind, Validator, name, setting_key};
use erp_fields::{Error, Result, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const BASE_KEY_FAMILIES: [&str; 3] = ["attachments", "password", "sessions"];

// A number whose key's last part contains one of these words is a limit, size, count, timeout,
// batch, interval or amount of bytes: its floor is at least 1, so 0 can never mean "unlimited".
const QUANTITY_WORDS: [&str; 9] = [
    "limit", "size", "count", "timeout", "batch", "bytes", "secs", "seconds", "interval",
];

/// Longest stored text a parameter of any kind is parsed from; longer text is refused unread.
const MAX_TEXT_BYTES: usize = 65_536;

fn bad(key: &str, why: &str) -> Error {
    Error::validation(format!("Parameter {key} {why}."))
}

// Checked before any message names the key, so a malformed key is never echoed.
fn checked_key(key: &str) -> Result<()> {
    if setting_key(key) {
        return Ok(());
    }
    Err(Error::validation(
        "A parameter key must be lowercase dotted names of letters, digits and _, at most 128 bytes.",
    ))
}

/// One declared technical parameter: key, label, kind with its floor and hard bound, default, an
/// optional pure validator and whether the value is a secret.
///
/// Built only through the validating constructors; the registry binds it to its addon at
/// publication ([`ParameterRegistry::build`]).
///
/// ```
/// use erp_registry::settings::ParameterDecl;
/// let batch = ParameterDecl::integer("my_addon.import_batch", "Import batch", 500, (1, 10_000))?;
/// assert_eq!(batch.default_text(), "500");
/// assert!(ParameterDecl::integer("my_addon.import_batch", "Import batch", 0, (0, 10_000)).is_err());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Clone)]
pub struct ParameterDecl {
    key: String,
    label: String,
    kind: SettingKind,
    default: Value,
    default_text: String,
    validator: Option<Validator>,
    secret: bool,
}
impl std::fmt::Debug for ParameterDecl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The default of a secret parameter is never printed.
        f.debug_struct("ParameterDecl")
            .field("key", &self.key)
            .field("kind", &self.kind)
            .field("secret", &self.secret)
            .finish_non_exhaustive()
    }
}
impl ParameterDecl {
    fn new(key: &str, label: &str, kind: SettingKind, default: Value) -> Result<Self> {
        checked_key(key)?;
        if label.is_empty() || label.len() > 256 || label.chars().any(char::is_control) {
            return Err(bad(
                key,
                "needs a label of 1 to 256 bytes without control characters",
            ));
        }
        let default_text = match canonical(&default) {
            Some(text) if kind.fits(&default) => text,
            _ => {
                return Err(bad(
                    key,
                    &format!("has a default outside {}", kind.describe()),
                ));
            }
        };
        Ok(Self {
            key: key.into(),
            label: label.into(),
            kind,
            default,
            default_text,
            validator: None,
            secret: false,
        })
    }
    // floor <= bound, and floor >= 1 for quantities ("Bounds rule").
    fn bounds(key: &str, floor: f64, bound: f64) -> Result<()> {
        checked_key(key)?;
        if !floor.is_finite() || !bound.is_finite() || floor > bound {
            return Err(bad(key, "needs a finite floor not above its hard bound"));
        }
        let last = key.rsplit('.').next().unwrap_or(key);
        if floor < 1.0 && QUANTITY_WORDS.iter().any(|word| last.contains(word)) {
            return Err(bad(
                key,
                "is a limit, size, count, timeout, batch or interval; its floor must be at least 1",
            ));
        }
        Ok(())
    }
    /// Declare a boolean parameter.
    /// # Errors
    /// `Validation` for a malformed key or label.
    pub fn boolean(key: &str, label: &str, default: bool) -> Result<Self> {
        Self::new(key, label, SettingKind::Boolean, Value::Bool(default))
    }
    /// Declare a text parameter of at most `max_bytes` bytes (1 to 65,536).
    /// # Errors
    /// `Validation` naming the key for a bound outside 1 to 65,536 or a longer default.
    pub fn string(key: &str, label: &str, default: &str, max_bytes: usize) -> Result<Self> {
        checked_key(key)?;
        if !(1..=MAX_TEXT_BYTES).contains(&max_bytes) {
            return Err(bad(key, "needs a text bound of 1 to 65536 bytes"));
        }
        Self::new(
            key,
            label,
            SettingKind::String { max_bytes },
            Value::str(default),
        )
    }
    /// Declare a whole number with `(floor, hard bound)`, both inclusive and JSON-safe.
    /// # Errors
    /// `Validation` naming the key when `floor <= default <= bound` does not hold, a bound is not
    /// JSON-safe, or the key names a quantity and the floor is below 1.
    pub fn integer(key: &str, label: &str, default: i64, bounds: (i64, i64)) -> Result<Self> {
        const SAFE: i64 = 9_007_199_254_740_991;
        checked_key(key)?;
        let (min, max) = bounds;
        if min < -SAFE || max > SAFE {
            return Err(bad(key, "needs bounds within the JSON-safe integers"));
        }
        #[allow(
            clippy::cast_precision_loss,
            reason = "JSON-safe integers are exact in f64"
        )]
        Self::bounds(key, min as f64, max as f64)?;
        Self::new(
            key,
            label,
            SettingKind::Integer { min, max },
            Value::Int(default),
        )
    }
    /// Declare a finite number with `(floor, hard bound)`, both inclusive.
    /// # Errors
    /// As [`Self::integer`], and for a bound that is not finite.
    pub fn float(key: &str, label: &str, default: f64, bounds: (f64, f64)) -> Result<Self> {
        let (min, max) = bounds;
        Self::bounds(key, min, max)?;
        Self::new(
            key,
            label,
            SettingKind::Float { min, max },
            Value::Float(default),
        )
    }
    /// Declare a choice among unique keys (1 to 256 choices, labels in display order).
    /// # Errors
    /// `Validation` naming the key for empty, duplicate or malformed choices or a default that is
    /// not a choice.
    pub fn selection(
        key: &str,
        label: &str,
        default: &str,
        choices: &[(&str, &str)],
    ) -> Result<Self> {
        let mut unique = BTreeSet::new();
        if choices.is_empty()
            || choices.len() > 256
            || choices.iter().any(|(choice, text)| {
                choice.is_empty()
                    || choice.len() > 128
                    || choice.chars().any(|c| c.is_control() || c.is_whitespace())
                    || text.is_empty()
                    || text.len() > 256
                    || text.chars().any(char::is_control)
                    || !unique.insert(*choice)
            })
        {
            return Err(bad(
                key,
                "needs 1 to 256 unique choices without spaces or control characters",
            ));
        }
        Self::new(
            key,
            label,
            SettingKind::Selection {
                choices: choices
                    .iter()
                    .map(|(k, v)| ((*k).into(), (*v).into()))
                    .collect(),
            },
            Value::str(default),
        )
    }
    /// Add a pure validator, run after the kind and bounds check; it must accept the default.
    /// # Errors
    /// The validator's error for the default.
    pub fn validator(mut self, validator: Validator) -> Result<Self> {
        validator(&self.default)?;
        self.validator = Some(validator);
        Ok(self)
    }
    /// Mark the value as a secret (a credential): access rules and audit never show it.
    #[must_use]
    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }
    /// The `ir.config_parameter` key.
    pub fn key(&self) -> &str {
        &self.key
    }
    /// The label shown next to the key in System Parameters.
    pub fn label(&self) -> &str {
        &self.label
    }
    /// The kind, with the floor and hard bound of a number.
    pub fn kind(&self) -> &SettingKind {
        &self.kind
    }
    /// The value a missing or empty row means.
    pub fn default_value(&self) -> &Value {
        &self.default
    }
    /// Whether the value is a secret.
    pub fn is_secret(&self) -> bool {
        self.secret
    }
    /// The canonical text of the default, as its default row stores it.
    pub fn default_text(&self) -> &str {
        &self.default_text
    }
    /// Check text proposed for this key on any write path. `None` or empty is the default.
    /// # Errors
    /// `Validation` `<key> takes <bounds>.` for text of the wrong kind or out of bounds (the text
    /// is not echoed); the validator's error.
    pub fn check_text(&self, text: Option<&str>) -> Result<Value> {
        let refuse = || Error::validation(format!("{} takes {}.", self.key, self.kind.describe()));
        let value = match text.filter(|t| !t.is_empty()) {
            None => return Ok(self.default.clone()),
            Some(text) => self.parse(text).ok_or_else(refuse)?,
        };
        if !self.kind.fits(&value) {
            return Err(refuse());
        }
        if let Some(validator) = self.validator {
            validator(&value)?;
        }
        Ok(value)
    }
    /// Read stored text for this key. `None` or empty is the default.
    /// # Errors
    /// `Validation` `Stored parameter <key> is invalid.` for any text [`Self::check_text`] refuses;
    /// the consumer fails, it never falls back to the default.
    pub fn read_text(&self, text: Option<&str>) -> Result<Value> {
        self.check_text(text)
            .map_err(|_| Error::validation(format!("Stored parameter {} is invalid.", self.key)))
    }
    fn parse(&self, text: &str) -> Option<Value> {
        if text.len() > MAX_TEXT_BYTES {
            return None;
        }
        match &self.kind {
            SettingKind::Boolean => match text.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" | "1" | "true" | "t" | "on" => Some(Value::Bool(true)),
                "n" | "no" | "0" | "false" | "f" | "off" => Some(Value::Bool(false)),
                _ => None,
            },
            SettingKind::Integer { .. } => text.trim().parse::<i64>().ok().map(Value::Int),
            SettingKind::Float { .. } => text
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .map(Value::Float),
            SettingKind::String { .. } | SettingKind::Selection { .. } => Some(Value::str(text)),
        }
    }
}

/// The canonical text of a parameter value: decimal integer, shortest round-trip float (with a
/// decimal point), `True`/`False`, text as is.
fn canonical(value: &Value) -> Option<String> {
    match value {
        Value::Int(v) => Some(v.to_string()),
        Value::Float(v) => Some(format!("{v:?}")),
        Value::Bool(v) => Some(if *v { "True" } else { "False" }.to_owned()),
        Value::Str(v) => Some(v.to_string()),
        _ => None,
    }
}

/// One addon's declared parameters and parameter groups, as published.
pub type ParameterContributions = Vec<(String, Vec<ParameterDecl>, Vec<SettingGroupDecl>)>;

/// The parameters of the installed addons, bound to their owners, with their groups and the key
/// prefixes they reserve. Absence of a key is explicit.
#[derive(Debug, Default)]
pub struct ParameterRegistry {
    parameters: BTreeMap<String, (String, ParameterDecl)>,
    groups: BTreeMap<String, SettingGroupDecl>,
    reserved: BTreeSet<String>,
    taken: BTreeSet<String>,
}
// Whether `owner` may declare `key`: its own name, and base's families for base.
fn owns(owner: &str, key: &str) -> bool {
    let family = key.split_once('.').map_or(key, |(family, _)| family);
    family == owner || (owner == "base" && BASE_KEY_FAMILIES.contains(&family))
}
impl ParameterRegistry {
    /// Bind each installed addon's parameters and groups. `taken` holds the keys of every
    /// published setting and Settings page item (one key space).
    /// # Errors
    /// `Validation` naming the key: a key declared twice or also taken, a key outside the
    /// addon's prefix, a group member that is not a parameter of the same addon, a group declared
    /// twice, a group validator refusing the defaults; `Validation` for an uninstalled owner.
    pub fn build(
        contributions: ParameterContributions,
        installed: &BTreeSet<String>,
        taken: BTreeSet<String>,
    ) -> Result<Self> {
        let mut registry = Self {
            taken,
            ..Self::default()
        };
        for (owner, declarations, groups) in contributions {
            if !installed.contains(&owner) || !name(&owner) {
                return Err(Error::validation(format!(
                    "Parameters of {owner} are contributed but the addon is not installed."
                )));
            }
            if !declarations.is_empty() {
                registry.reserved.insert(owner.clone());
                if owner == "base" {
                    registry
                        .reserved
                        .extend(BASE_KEY_FAMILIES.iter().map(|f| (*f).to_owned()));
                }
            }
            for decl in declarations {
                if !owns(&owner, &decl.key) {
                    return Err(bad(
                        &decl.key,
                        &format!("is outside the key prefix of addon {owner}"),
                    ));
                }
                if registry.taken.contains(&decl.key) || registry.parameters.contains_key(&decl.key)
                {
                    return Err(bad(&decl.key, "has more than one declaration"));
                }
                registry
                    .parameters
                    .insert(decl.key.clone(), (owner.clone(), decl));
            }
            for group in groups {
                registry.add_group(&owner, group)?;
            }
        }
        Ok(registry)
    }
    fn add_group(&mut self, owner: &str, group: SettingGroupDecl) -> Result<()> {
        let refuse = |why: &str| Error::validation(format!("Parameter group {} {why}.", group.key));
        if !owns(owner, &group.key) || self.groups.contains_key(&group.key) {
            return Err(refuse(
                "is outside its addon or has more than one declaration",
            ));
        }
        let mut defaults = BTreeMap::new();
        for member in &group.members {
            match self.parameters.get(member) {
                Some((member_owner, decl)) if member_owner == owner => {
                    defaults.insert(member.clone(), decl.default.clone());
                }
                _ => {
                    return Err(refuse(&format!(
                        "names {member}, not a parameter of {owner}"
                    )));
                }
            }
        }
        (group.validate)(&defaults)?;
        self.groups.insert(group.key.clone(), group);
        Ok(())
    }
    /// The declaration of `key`, when an installed addon declares it.
    pub fn get(&self, key: &str) -> Option<&ParameterDecl> {
        self.parameters.get(key).map(|(_, decl)| decl)
    }
    /// The addon that declares `key`.
    pub fn owner(&self, key: &str) -> Option<&str> {
        self.parameters.get(key).map(|(owner, _)| owner.as_str())
    }
    /// The declarations, sorted by key.
    pub fn iter(&self) -> impl Iterator<Item = &ParameterDecl> {
        self.parameters.values().map(|(_, decl)| decl)
    }
    /// Refuse `key` when it is not declared but lies under the prefix of an installed addon that
    /// declares parameters. Setting and page item keys count as declared; any other
    /// key is free text.
    /// # Errors
    /// `Validation` `Parameter <key> is not declared by an installed addon.`
    pub fn refuse_undeclared(&self, key: &str) -> Result<()> {
        let family = key.split_once('.').map_or(key, |(family, _)| family);
        if self.reserved.contains(family)
            && !self.parameters.contains_key(key)
            && !self.taken.contains(key)
        {
            return Err(bad(key, "is not declared by an installed addon"));
        }
        Ok(())
    }
    /// Every member of every group that contains one of `keys`, followed through overlapping
    /// groups, sorted. Empty when no key is a group member.
    pub fn group_members<'a>(&self, keys: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut found: BTreeSet<String> = BTreeSet::new();
        let mut pending: Vec<String> = keys.into_iter().map(str::to_owned).collect();
        while let Some(key) = pending.pop() {
            for group in self.groups.values() {
                if group.members.contains(&key) {
                    for member in &group.members {
                        if found.insert(member.clone()) {
                            pending.push(member.clone());
                        }
                    }
                }
            }
        }
        found.into_iter().collect()
    }
    /// Run the validator of every group that has a member in `values`.
    /// # Errors
    /// `Programming` when a touched group is not complete in `values`; the validator's error.
    pub fn validate_groups(&self, values: &BTreeMap<String, Value>) -> Result<()> {
        for group in self.groups.values() {
            if !group.members.iter().any(|key| values.contains_key(key)) {
                continue;
            }
            let mut snapshot = BTreeMap::new();
            for member in &group.members {
                let value = values.get(member).ok_or_else(|| {
                    Error::programming(format!(
                        "Parameter group {} is checked without its member {member}.",
                        group.key
                    ))
                })?;
                snapshot.insert(member.clone(), value.clone());
            }
            (group.validate)(&snapshot)?;
        }
        Ok(())
    }
}
