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
impl SettingKind {
    /// Whether `value` has this kind and lies inside its bounds (no coercion).
    #[must_use]
    pub fn fits(&self, value: &Value) -> bool {
        match (self, value) {
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
        }
    }
    /// What a value of this kind is, with its bounds, for a refusal: "a whole number from 1 to
    /// 32". Never contains the refused value, which may be a secret.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            SettingKind::Boolean => "true or false".to_owned(),
            SettingKind::String { max_bytes } => format!("a text of at most {max_bytes} bytes"),
            SettingKind::Integer { min, max } => format!("a whole number from {min} to {max}"),
            SettingKind::Float { min, max } => format!("a number from {min} to {max}"),
            SettingKind::Selection { choices } => format!(
                "one of {}",
                choices
                    .iter()
                    .map(|(key, _)| key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
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
    help: Option<String>,
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
fn setting_key(key: &str) -> bool {
    key.len() <= 128 && key.contains('.') && key.split('.').all(name)
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
        if !setting_key(key)
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
            help: None,
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
        if !self.kind.fits(value) {
            // One sentence for every refusal of a value, naming the setting and what it takes.
            return Err(Error::validation(format!(
                "{} takes {}.",
                self.key,
                self.kind.describe()
            )));
        }
        if let Some(validator) = self.validator {
            validator(value)?;
        }
        Ok(())
    }
    /// Add bounded descriptive help, including units and catalogue-derived bounds.
    /// # Errors
    /// Invalid, empty or oversized text.
    pub fn help(mut self, text: &str) -> Result<Self> {
        if text.is_empty() || text.len() > 1024 || text.chars().any(char::is_control) {
            return Err(invalid());
        }
        self.help = Some(text.into());
        Ok(self)
    }
    /// Optional explanatory text; never interpreted as markup.
    pub fn help_text(&self) -> Option<&str> {
        self.help.as_deref()
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
/// Maximum settings in a pure validation group and in one expanded service snapshot.
pub const MAX_GROUP_MEMBERS: usize = 256;
/// Pure validation of a complete typed group. No database or caller capability is passed.
pub type GroupValidator = fn(&BTreeMap<String, Value>) -> Result<()>;
/// Validated native group metadata, bound to its installed declaring addon at publication.
///
/// ```
/// use erp_registry::settings::SettingGroupDecl;
/// let group = SettingGroupDecl::new("my_addon.limits", &["my_addon.low", "my_addon.high"], |_values| Ok(()))?;
/// assert_eq!(group.key(), "my_addon.limits");
/// assert_eq!(group.members().count(), 2);
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct SettingGroupDecl {
    key: String,
    members: BTreeSet<String>,
    validate: GroupValidator,
}
impl std::fmt::Debug for SettingGroupDecl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingGroupDecl")
            .field("key", &self.key)
            .field("members", &self.members)
            .finish_non_exhaustive()
    }
}
fn invalid_group() -> Error {
    Error::validation("invalid declared setting group")
}
impl SettingGroupDecl {
    /// Declare unique same-addon members, checked for existence/scope/defaults at publication.
    /// # Errors
    /// Invalid identity, duplicate/empty/oversized membership or a member owned by another addon.
    pub fn new(key: &str, members: &[&str], validate: GroupValidator) -> Result<Self> {
        let owner = key.split_once('.').ok_or_else(invalid_group)?.0;
        if !setting_key(key) || members.is_empty() || members.len() > MAX_GROUP_MEMBERS {
            return Err(invalid_group());
        }
        let mut unique = BTreeSet::new();
        for member in members {
            if !setting_key(member)
                || member.split_once('.').map(|p| p.0) != Some(owner)
                || !unique.insert((*member).to_owned())
            {
                return Err(invalid_group());
            }
        }
        Ok(Self {
            key: key.into(),
            members: unique,
            validate,
        })
    }
    /// Stable group identity.
    pub fn key(&self) -> &str {
        &self.key
    }
    /// All group keys in deterministic order.
    pub fn members(&self) -> impl Iterator<Item = &str> {
        self.members.iter().map(String::as_str)
    }
}
/// Immutable installed-addon declarations: settings, and the panel items published next to them.
/// No secret setting kind exists in this contract.
#[derive(Debug, Default)]
pub struct SettingsRegistry {
    settings: BTreeMap<String, SettingDecl>,
    items: BTreeMap<String, PanelItemDecl>,
    groups: BTreeMap<String, SettingGroupDecl>,
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
            groups: BTreeMap::new(),
        })
    }
    /// Bind pure groups to installed owners and validate their complete declared defaults.
    /// # Errors
    /// Invalid owner, duplicate group, absent member, mixed scope or invalid default snapshot.
    pub fn with_groups(
        mut self,
        contributions: Vec<(String, Vec<SettingGroupDecl>)>,
        installed: &BTreeSet<String>,
    ) -> Result<Self> {
        for (owner, groups) in contributions {
            if !installed.contains(&owner) {
                return Err(invalid_group());
            }
            for group in groups {
                if group.key.split_once('.').map(|p| p.0) != Some(owner.as_str())
                    || self.groups.contains_key(&group.key)
                {
                    return Err(invalid_group());
                }
                let mut scope = None;
                let mut defaults = BTreeMap::new();
                for member in &group.members {
                    let decl = self.get(member).ok_or_else(invalid_group)?;
                    if scope.is_some_and(|old| old != decl.scope()) {
                        return Err(invalid_group());
                    }
                    scope = Some(decl.scope());
                    defaults.insert(member.clone(), decl.default_value().clone());
                }
                (group.validate)(&defaults)?;
                self.groups.insert(group.key.clone(), group);
            }
        }
        Ok(self)
    }
    /// Expand requested keys to complete affected groups (including overlapping dependencies), without changing caller authority.
    /// # Errors
    /// The expanded snapshot exceeds the bounded service admission size.
    pub fn expanded_keys(&self, keys: &[String]) -> Result<Vec<String>> {
        let mut expanded: BTreeSet<String> = keys.iter().cloned().collect();
        loop {
            let before = expanded.len();
            for group in self.groups.values() {
                if group.members.iter().any(|key| expanded.contains(key)) {
                    expanded.extend(group.members.iter().cloned());
                    if expanded.len() > MAX_GROUP_MEMBERS {
                        return Err(invalid_group());
                    }
                }
            }
            if expanded.len() == before {
                break;
            }
        }
        if expanded.len() > MAX_GROUP_MEMBERS {
            return Err(invalid_group());
        }
        Ok(expanded.into_iter().collect())
    }
    /// Validate every fully supplied group, rejecting an incomplete affected snapshot.
    /// # Errors
    /// Missing members or a native group validation error. Overlapping groups are checked once.
    pub fn validate_groups(&self, values: &BTreeMap<String, Value>) -> Result<()> {
        for group in self.groups.values() {
            if group.members.iter().any(|key| values.contains_key(key)) {
                if !group.members.iter().all(|key| values.contains_key(key)) {
                    return Err(invalid_group());
                }
                let snapshot = group
                    .members
                    .iter()
                    .map(|key| {
                        values
                            .get(key)
                            .cloned()
                            .map(|value| (key.clone(), value))
                            .ok_or_else(invalid_group)
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?;
                (group.validate)(&snapshot)?;
            }
        }
        Ok(())
    }
    /// Whether the complete required group was linked and published.
    pub fn group(&self, key: &str) -> Option<&SettingGroupDecl> {
        self.groups.get(key)
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
