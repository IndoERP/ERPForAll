//! Native, addon-owned settings declarations.
use erp_fields::{Error, RecId, Result, Value};
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
    app: String,
    block: String,
    sequence: u32,
}
impl std::fmt::Debug for SettingDecl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingDecl")
            .field("key", &self.key)
            .field("scope", &self.scope)
            .field("kind", &self.kind)
            .field("app", &self.app)
            .field("block", &self.block)
            .field("sequence", &self.sequence)
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
        let default_app = key
            .split_once('.')
            .map(|(prefix, _)| prefix.to_string())
            .ok_or_else(invalid)?;
        let setting = Self {
            key: key.into(),
            label: label.into(),
            scope,
            kind,
            default,
            read_groups: Vec::new(),
            write_groups: Vec::new(),
            validator: None,
            app: default_app,
            block: "General".into(),
            sequence: 100,
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
    /// Set the target application / settings page.
    /// # Errors
    /// Invalid addon identifier.
    pub fn app(mut self, app: &str) -> Result<Self> {
        if !name(app) || app.len() > 128 {
            return Err(invalid());
        }
        self.app = app.into();
        Ok(self)
    }
    /// Alias for `app`.
    /// # Errors
    /// Invalid addon identifier.
    pub fn with_app(self, app: &str) -> Result<Self> {
        self.app(app)
    }
    /// Set the section or block title within the settings page.
    /// # Errors
    /// Empty, oversized (> 256 bytes) or containing control characters.
    pub fn block(mut self, block: &str) -> Result<Self> {
        if block.is_empty() || block.len() > 256 || block.chars().any(char::is_control) {
            return Err(invalid());
        }
        self.block = block.into();
        Ok(self)
    }
    /// Alias for `block`.
    /// # Errors
    /// Empty, oversized (> 256 bytes) or containing control characters.
    pub fn with_block(self, block: &str) -> Result<Self> {
        self.block(block)
    }
    /// Set the display sort sequence.
    #[must_use]
    pub fn sequence(mut self, sequence: u32) -> Self {
        self.sequence = sequence;
        self
    }
    /// Alias for `sequence`.
    #[must_use]
    pub fn with_sequence(self, sequence: u32) -> Self {
        self.sequence(sequence)
    }
    /// Target application settings page.
    pub fn target_app(&self) -> &str {
        &self.app
    }
    /// Section or block title within the settings page.
    pub fn block_title(&self) -> &str {
        &self.block
    }
    /// Display sort sequence.
    pub fn display_sequence(&self) -> u32 {
        self.sequence
    }
}
/// External identity of a window action or menu that a link row opens.
///
/// Built only by the [`PanelItemDecl`] link constructors, which validate the identity; registry publication
/// resolves it to exactly one row owned by an addon in the contributor's dependency closure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkTarget {
    /// An `ir.actions.act_window` identity; `record` asks the resolver for the record the action opens.
    #[non_exhaustive]
    Action {
        /// `<module>.<name>` identity of the action.
        xmlid: String,
        /// Whether the link opens one record that the declaring addon's resolver names.
        record: bool,
    },
    /// An `ir.ui.menu` identity.
    #[non_exhaustive]
    Menu {
        /// `<module>.<name>` identity of the menu.
        xmlid: String,
    },
}
impl LinkTarget {
    /// The `<module>.<name>` identity, whatever the target kind.
    pub fn xmlid(&self) -> &str {
        match self {
            Self::Action { xmlid, .. } | Self::Menu { xmlid } => xmlid,
        }
    }
}
/// What a panel item shows: a read-only value or a link.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PanelItemKind {
    /// A read-only value computed by the declaring addon's resolver.
    Info,
    /// An arrow link to a window action or a menu.
    Link(LinkTarget),
}
/// A read-only information row or a link row on a Settings app page: a sibling of [`SettingDecl`], never a
/// setting. It has no default, no write groups and no stored value.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PanelItemDecl {
    key: String,
    label: String,
    scope: Option<ScopeKind>,
    kind: PanelItemKind,
    read_groups: Vec<String>,
    app: String,
    block: String,
    sequence: u32,
}
// `<module>.<name>`: module as an addon name, name of ASCII letters, digits and `_`, 256 bytes in total.
fn external_identity(xmlid: &str) -> bool {
    let Some((module, local)) = xmlid.split_once('.') else {
        return false;
    };
    xmlid.len() <= 256
        && name(module)
        && !local.is_empty()
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
impl PanelItemDecl {
    fn new(key: &str, label: &str, kind: PanelItemKind) -> Result<Self> {
        if !qualified(key)
            || label.is_empty()
            || label.len() > 256
            || label.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let app = key
            .split_once('.')
            .map(|(prefix, _)| prefix.to_owned())
            .ok_or_else(invalid)?;
        Ok(Self {
            key: key.into(),
            label: label.into(),
            scope: None,
            kind,
            read_groups: Vec::new(),
            app,
            block: "General".into(),
            sequence: 100,
        })
    }
    /// Declare a read-only information row; its value comes from the addon's resolver.
    /// # Errors
    /// Invalid addon-qualified key or label (1 to 256 bytes, no control characters).
    pub fn info(key: &str, label: &str) -> Result<Self> {
        Self::new(key, label, PanelItemKind::Info)
    }
    /// Declare a link to a window action; with `record` the resolver names the record it opens.
    /// # Errors
    /// Invalid key, label or action identity.
    pub fn action_link(key: &str, label: &str, action: &str, record: bool) -> Result<Self> {
        if !external_identity(action) {
            return Err(invalid());
        }
        Self::new(
            key,
            label,
            PanelItemKind::Link(LinkTarget::Action {
                xmlid: action.into(),
                record,
            }),
        )
    }
    /// Declare a link to a menu.
    /// # Errors
    /// Invalid key, label or menu identity.
    pub fn menu_link(key: &str, label: &str, menu: &str) -> Result<Self> {
        if !external_identity(menu) {
            return Err(invalid());
        }
        Self::new(
            key,
            label,
            PanelItemKind::Link(LinkTarget::Menu { xmlid: menu.into() }),
        )
    }
    /// Set the required read groups (ANY of a nonempty list of external identities).
    /// # Errors
    /// Empty, duplicate or malformed group references. Registry publication resolves each reference.
    pub fn access(mut self, read: &[&str]) -> Result<Self> {
        self.read_groups = policy(read)?;
        Ok(self)
    }
    /// Set the target application / settings page.
    /// # Errors
    /// Invalid addon identifier.
    pub fn app(mut self, app: &str) -> Result<Self> {
        if !name(app) || app.len() > 128 {
            return Err(invalid());
        }
        self.app = app.into();
        Ok(self)
    }
    /// Set the block title within the settings page.
    /// # Errors
    /// Empty, oversized (> 256 bytes) or containing control characters.
    pub fn block(mut self, block: &str) -> Result<Self> {
        if block.is_empty() || block.len() > 256 || block.chars().any(char::is_control) {
            return Err(invalid());
        }
        self.block = block.into();
        Ok(self)
    }
    /// Set the display sort sequence.
    #[must_use]
    pub fn sequence(mut self, sequence: u32) -> Self {
        self.sequence = sequence;
        self
    }
    /// Show the item in one scope only; without this it shows in every scope.
    #[must_use]
    pub fn scope(mut self, scope: ScopeKind) -> Self {
        self.scope = Some(scope);
        self
    }
    /// Addon-qualified identity, unique across settings and items.
    pub fn key(&self) -> &str {
        &self.key
    }
    /// Declared display label; an information value may override it (singular and plural).
    pub fn label(&self) -> &str {
        &self.label
    }
    /// Information row or link with its target.
    pub fn kind(&self) -> &PanelItemKind {
        &self.kind
    }
    /// Required read groups, resolved at publication.
    pub fn read_groups(&self) -> &[String] {
        &self.read_groups
    }
    /// The one scope the item is restricted to; `None` shows it in every scope.
    pub fn scope_restriction(&self) -> Option<ScopeKind> {
        self.scope
    }
    /// Whether the item shows in `scope`.
    pub fn shows_in(&self, scope: ScopeKind) -> bool {
        self.scope.is_none_or(|only| only == scope)
    }
    /// Target application settings page.
    pub fn target_app(&self) -> &str {
        &self.app
    }
    /// Block title within the settings page.
    pub fn block_title(&self) -> &str {
        &self.block
    }
    /// Display sort sequence.
    pub fn display_sequence(&self) -> u32 {
        self.sequence
    }
}
const ITEM_SCALAR: &str = "A setting item value must be a boolean, text, integer or finite float.";
const ITEM_LABEL: &str = "A setting item label must be 1 to 256 bytes without control characters.";
/// Largest detail text, in bytes, that an information value may carry ([`PanelItemValue::detail`]).
pub const MAX_ITEM_DETAIL_BYTES: usize = 1024;
fn item_detail_message() -> String {
    format!(
        "A setting item detail must be at most {MAX_ITEM_DETAIL_BYTES} bytes; a line feed is its only \
         allowed control character."
    )
}
const ITEM_NOT_INFO: &str = "Only an information value carries a label or detail.";
/// The value a resolver returns for one panel item.
///
/// Built only through its validating constructors; a value that breaks a bound is a programming error of the
/// resolver and is never truncated.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PanelItemValue {
    /// The value of an information row.
    #[non_exhaustive]
    Info {
        /// Boolean, text, integer or finite float.
        value: Value,
        /// Label replacing the declared one (for example the singular form of a count).
        label: Option<String>,
        /// Secondary text under the value; line feeds separate lines.
        detail: Option<String>,
    },
    /// The stored record a record link opens.
    #[non_exhaustive]
    Record {
        /// Database id of the record.
        id: RecId,
    },
}
impl PanelItemValue {
    /// An information value.
    /// # Errors
    /// `Programming` unless `value` is a boolean, text, integer or finite float.
    pub fn info(value: Value) -> Result<Self> {
        let scalar = matches!(&value, Value::Bool(_) | Value::Str(_) | Value::Int(_))
            || matches!(&value, Value::Float(float) if float.is_finite());
        if !scalar {
            return Err(Error::programming(ITEM_SCALAR));
        }
        Ok(Self::Info {
            value,
            label: None,
            detail: None,
        })
    }
    /// Replace the declared label for this value.
    /// # Errors
    /// `Programming` for an empty, oversized (> 256 bytes) or control-character label, or on a record value.
    pub fn label(self, text: &str) -> Result<Self> {
        let Self::Info { value, detail, .. } = self else {
            return Err(Error::programming(ITEM_NOT_INFO));
        };
        if text.is_empty() || text.len() > 256 || text.chars().any(char::is_control) {
            return Err(Error::programming(ITEM_LABEL));
        }
        Ok(Self::Info {
            value,
            label: Some(text.into()),
            detail,
        })
    }
    /// Add a secondary text under the value.
    /// # Errors
    /// `Programming` for more than 1024 bytes or a control character other than line feed, or on a record value.
    pub fn detail(self, text: &str) -> Result<Self> {
        let Self::Info { value, label, .. } = self else {
            return Err(Error::programming(ITEM_NOT_INFO));
        };
        if text.len() > MAX_ITEM_DETAIL_BYTES || text.chars().any(|c| c.is_control() && c != '\n') {
            return Err(Error::programming(item_detail_message()));
        }
        Ok(Self::Info {
            value,
            label,
            detail: Some(text.into()),
        })
    }
    /// The record a record link opens.
    /// # Errors
    /// `Programming` for an in-memory new record.
    pub fn record(id: RecId) -> Result<Self> {
        if id.is_new() {
            return Err(Error::programming(
                "A setting item record must be a stored record.",
            ));
        }
        Ok(Self::Record { id })
    }
}
/// Immutable installed-addon declarations: settings, and the panel items published next to them.
/// No secret setting kind exists in this contract.
#[derive(Debug, Default)]
pub struct SettingsRegistry {
    settings: BTreeMap<String, SettingDecl>,
    items: BTreeMap<String, PanelItemDecl>,
}
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
                    || !installed.contains(decl.target_app())
                {
                    return Err(invalid());
                }
                let key = decl.key.clone();
                if entries.insert(key, decl).is_some() {
                    return Err(invalid());
                }
            }
        }
        Ok(Self {
            settings: entries,
            items: BTreeMap::new(),
        })
    }
    /// Add the installed panel items under the same ownership rule as settings.
    ///
    /// `target_exists(owner, target)` answers whether a link target of the contributing addon `owner` resolved
    /// when the registry was published (exactly one row, owned by an addon in `owner`'s dependency closure).
    /// # Errors
    /// `Validation` "invalid declared setting or value" for a wrong or uninstalled owner, a key already used by a
    /// setting or an item, empty or missing read groups or an uninstalled target app; `Validation` "Setting item
    /// references an unavailable action or menu." for a link whose target does not exist.
    pub fn with_items(
        mut self,
        contributions: Vec<(String, Vec<PanelItemDecl>)>,
        installed: &BTreeSet<String>,
        group_exists: impl Fn(&str) -> bool,
        target_exists: impl Fn(&str, &LinkTarget) -> bool,
    ) -> Result<Self> {
        for (owner, declarations) in contributions {
            if !installed.contains(&owner) || !name(&owner) {
                return Err(invalid());
            }
            for item in declarations {
                if item.key.split_once('.').map(|(prefix, _)| prefix) != Some(owner.as_str())
                    || item.read_groups.is_empty()
                    || item.read_groups.iter().any(|group| !group_exists(group))
                    || !installed.contains(item.target_app())
                    || self.settings.contains_key(&item.key)
                    || self.items.contains_key(&item.key)
                {
                    return Err(invalid());
                }
                if let PanelItemKind::Link(target) = &item.kind
                    && !target_exists(&owner, target)
                {
                    return Err(Error::validation(
                        "Setting item references an unavailable action or menu.",
                    ));
                }
                self.items.insert(item.key.clone(), item);
            }
        }
        Ok(self)
    }
    /// Look up a currently installed setting declaration; absence is explicit. Items are never returned.
    pub fn get(&self, key: &str) -> Option<&SettingDecl> {
        self.settings.get(key)
    }
    /// Setting declarations sorted by key.
    pub fn iter(&self) -> impl Iterator<Item = &SettingDecl> {
        self.settings.values()
    }
    /// Look up a published panel item; absence is explicit.
    pub fn item(&self, key: &str) -> Option<&PanelItemDecl> {
        self.items.get(key)
    }
    /// Published panel items sorted by key.
    pub fn items(&self) -> impl Iterator<Item = &PanelItemDecl> {
        self.items.values()
    }
}
