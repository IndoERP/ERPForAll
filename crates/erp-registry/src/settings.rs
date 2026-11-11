//! Native, addon-owned settings declarations.
use erp_fields::{Error, Result, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Scope vocabulary. Permission checks are independent of this declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScopeKind {
    /// Database-local values.
    Global,
    /// Company-local values.
    Company,
}
/// Declared scalar shape and intrinsic bounds. Stored only after constructor validation.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SettingKind {
    /// Boolean, with no string/integer coercion.
    Boolean,
    /// UTF-8 text bounded in bytes.
    String {
        /// Maximum encoded bytes.
        max_bytes: usize,
    },
    /// JSON-safe integer in an inclusive range.
    Integer {
        /// Lower bound.
        min: i64,
        /// Upper bound.
        max: i64,
    },
    /// Finite float in an inclusive range.
    Float {
        /// Lower bound.
        min: f64,
        /// Upper bound.
        max: f64,
    },
    /// Ordered, unique selection keys and labels.
    Selection {
        /// Choices in display order.
        choices: Vec<(String, String)>,
    },
}
/// Additional pure native validation after intrinsic kind/range validation.
pub type Validator = fn(&Value) -> Result<()>;
/// Validated declaration. A registry binds its owner to the contributing installed addon.
#[derive(Clone)]
#[non_exhaustive]
pub struct SettingDecl {
    key: String,
    label: String,
    scope: ScopeKind,
    kind: SettingKind,
    default: Value,
    read_groups: Vec<String>,
    write_groups: Vec<String>,
    validator: Option<Validator>,
}
impl std::fmt::Debug for SettingDecl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingDecl")
            .field("key", &self.key)
            .field("scope", &self.scope)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}
fn invalid() -> Error {
    Error::validation("invalid declared setting or value")
}
fn qualified(key: &str) -> bool {
    let Some((addon, local)) = key.split_once('.') else {
        return false;
    };
    key.len() <= 128 && name(addon) && name(local)
}
fn name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
fn policy(groups: &[&str]) -> Result<Vec<String>> {
    if groups.is_empty() || groups.len() > 64 {
        return Err(invalid());
    }
    let mut unique = BTreeSet::new();
    if groups
        .iter()
        .any(|group| !qualified(group) || !unique.insert(*group))
    {
        return Err(invalid());
    }
    Ok(groups.iter().map(|group| (*group).to_owned()).collect())
}
impl SettingDecl {
    fn new(
        key: &str,
        label: &str,
        scope: ScopeKind,
        kind: SettingKind,
        default: Value,
    ) -> Result<Self> {
        if !qualified(key)
            || label.is_empty()
            || label.len() > 256
            || label.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let setting = Self {
            key: key.into(),
            label: label.into(),
            scope,
            kind,
            default,
            read_groups: Vec::new(),
            write_groups: Vec::new(),
            validator: None,
        };
        setting.validate(&setting.default)?;
        Ok(setting)
    }
    /// Declare a boolean setting with a typed default.
    /// # Errors
    /// Invalid addon-qualified key, label or default.
    pub fn boolean(key: &str, label: &str, scope: ScopeKind, default: bool) -> Result<Self> {
        Self::new(
            key,
            label,
            scope,
            SettingKind::Boolean,
            Value::Bool(default),
        )
    }
    /// Declare text with an explicit positive bound (at most 64 KiB).
    /// # Errors
    /// Invalid declaration or default outside its bound.
    pub fn string(
        key: &str,
        label: &str,
        scope: ScopeKind,
        default: &str,
        max_bytes: usize,
    ) -> Result<Self> {
        if !(1..=65_536).contains(&max_bytes) {
            return Err(invalid());
        }
        Self::new(
            key,
            label,
            scope,
            SettingKind::String { max_bytes },
            Value::str(default),
        )
    }
    /// Declare a JSON-safe integer with inclusive bounds.
    /// # Errors
    /// Invalid bounds, declaration or default.
    pub fn integer(
        key: &str,
        label: &str,
        scope: ScopeKind,
        default: i64,
        bounds: (i64, i64),
    ) -> Result<Self> {
        let (min, max) = bounds;
        if min > max || min < -9_007_199_254_740_991 || max > 9_007_199_254_740_991 {
            return Err(invalid());
        }
        Self::new(
            key,
            label,
            scope,
            SettingKind::Integer { min, max },
            Value::Int(default),
        )
    }
    /// Declare a finite float with inclusive finite bounds.
    /// # Errors
    /// Nonfinite or inverted bounds, invalid declaration or default.
    pub fn float(
        key: &str,
        label: &str,
        scope: ScopeKind,
        default: f64,
        bounds: (f64, f64),
    ) -> Result<Self> {
        let (min, max) = bounds;
        if !min.is_finite() || !max.is_finite() || min > max {
            return Err(invalid());
        }
        Self::new(
            key,
            label,
            scope,
            SettingKind::Float { min, max },
            Value::Float(default),
        )
    }
    /// Declare an ordered selection of unique bounded keys and labels.
    /// # Errors
    /// Empty/duplicate/malformed choices or undeclared default.
    pub fn selection(
        key: &str,
        label: &str,
        scope: ScopeKind,
        default: &str,
        choices: &[(&str, &str)],
    ) -> Result<Self> {
        if choices.is_empty() || choices.len() > 256 {
            return Err(invalid());
        }
        let mut unique = BTreeSet::new();
        if choices.iter().any(|(key, label)| {
            key.is_empty()
                || key.len() > 128
                || key.chars().any(char::is_control)
                || label.is_empty()
                || label.len() > 256
                || label.chars().any(char::is_control)
                || !unique.insert(*key)
        }) {
            return Err(invalid());
        }
        Self::new(
            key,
            label,
            scope,
            SettingKind::Selection {
                choices: choices
                    .iter()
                    .map(|(k, v)| ((*k).into(), (*v).into()))
                    .collect(),
            },
            Value::str(default),
        )
    }
    /// Set required read/write group external identities (ANY within each nonempty list).
    /// # Errors
    /// Empty, duplicate or malformed policy references. Registry publication resolves each reference.
    pub fn access(mut self, read: &[&str], write: &[&str]) -> Result<Self> {
        self.read_groups = policy(read)?;
        self.write_groups = policy(write)?;
        Ok(self)
    }
    /// Add a pure native validator, checking the default before publication.
    /// # Errors
    /// The validator refuses the default.
    pub fn validator(mut self, validator: Validator) -> Result<Self> {
        validator(&self.default)?;
        self.validator = Some(validator);
        Ok(self)
    }
    /// Validate a proposed scalar without substituting a default or coercing its type.
    /// # Errors
    /// Wrong type, intrinsic bound violation or custom validation failure.
    pub fn validate(&self, value: &Value) -> Result<()> {
        let valid = match (&self.kind, value) {
            (SettingKind::Boolean, Value::Bool(_)) => true,
            (SettingKind::String { max_bytes }, Value::Str(value)) => value.len() <= *max_bytes,
            (SettingKind::Integer { min, max }, Value::Int(value)) => value >= min && value <= max,
            (SettingKind::Float { min, max }, Value::Float(value)) => {
                value.is_finite() && value >= min && value <= max
            }
            (SettingKind::Selection { choices }, Value::Str(value)) => {
                choices.iter().any(|(key, _)| key == value.as_ref())
            }
            _ => false,
        };
        if !valid {
            return Err(invalid());
        }
        if let Some(validator) = self.validator {
            validator(value)?;
        }
        Ok(())
    }
    /// Addon-qualified identity.
    pub fn key(&self) -> &str {
        &self.key
    }
    /// Public display label.
    pub fn label(&self) -> &str {
        &self.label
    }
    /// Exact value scope.
    pub fn scope(&self) -> ScopeKind {
        self.scope
    }
    /// Validated scalar shape.
    pub fn kind(&self) -> &SettingKind {
        &self.kind
    }
    /// Typed default used only when storage is absent.
    pub fn default_value(&self) -> &Value {
        &self.default
    }
    /// Required read groups, resolved at publication.
    pub fn read_groups(&self) -> &[String] {
        &self.read_groups
    }
    /// Required write groups, resolved at publication.
    pub fn write_groups(&self) -> &[String] {
        &self.write_groups
    }
}
/// Immutable installed-addon declarations. No secret setting kind exists in this contract.
#[derive(Debug, Default)]
pub struct SettingsRegistry(BTreeMap<String, SettingDecl>);
impl SettingsRegistry {
    /// Bind declarations to their actual contributing installed addon and resolve all policy references.
    /// # Errors
    /// Wrong owner, uninstalled owner, duplicate key or missing policy group.
    pub fn build(
        contributions: Vec<(String, Vec<SettingDecl>)>,
        installed: &BTreeSet<String>,
        group_exists: impl Fn(&str) -> bool,
    ) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for (owner, declarations) in contributions {
            if !installed.contains(&owner) || !name(&owner) {
                return Err(invalid());
            }
            for decl in declarations {
                if decl.key.split_once('.').map(|(prefix, _)| prefix) != Some(owner.as_str())
                    || decl.read_groups.is_empty()
                    || decl.write_groups.is_empty()
                    || decl
                        .read_groups
                        .iter()
                        .chain(&decl.write_groups)
                        .any(|group| !group_exists(group))
                {
                    return Err(invalid());
                }
                let key = decl.key.clone();
                if entries.insert(key, decl).is_some() {
                    return Err(invalid());
                }
            }
        }
        Ok(Self(entries))
    }
    /// Look up a currently installed declaration; absence is explicit.
    pub fn get(&self, key: &str) -> Option<&SettingDecl> {
        self.0.get(key)
    }
    /// Declarations sorted by key.
    pub fn iter(&self) -> impl Iterator<Item = &SettingDecl> {
        self.0.values()
    }
}
