//! Typed operations produced by the data-file readers.
//!
//! Operations are built only by the readers in this crate (XML: [`crate::XmlDataReader`], CSV:
//! [`crate::CsvImport::to_record_ops`]); their fields are private and read through accessors, so
//! every value a later slice consumes went through the reader's checks.

use std::fmt;
use std::sync::Arc;

/// A Python-like expression kept as source text (`eval`, `search`, `context` attributes).
///
/// evaluates it with `erp-expr`; the reader never evaluates anything. The
/// first use parses it ([`Expr::parsed`]); clones share that result, so an expression is parsed
/// once however many times the loader scans and evaluates it.
#[derive(Debug, Clone)]
pub struct Expr {
    source: String,
    parsed: std::sync::Arc<std::sync::OnceLock<Result<erp_expr::Expr, erp_expr::ExprError>>>,
}

impl PartialEq for Expr {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Eq for Expr {}

impl std::hash::Hash for Expr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.source.hash(state);
    }
}

impl Expr {
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            parsed: std::sync::Arc::default(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// The parsed expression, or why it does not parse; computed on the first call.
    pub fn parsed(&self) -> Result<&erp_expr::Expr, &erp_expr::ExprError> {
        self.parsed
            .get_or_init(|| erp_expr::parse(&self.source))
            .as_ref()
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

/// Why a string is not a valid external ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum XmlIdProblem {
    Empty,
    TooManyDots,
    EmptyModule,
    EmptyName,
    Whitespace,
    ControlCharacter,
    /// The module part is not a module name (ASCII letters, digits, `_`).
    InvalidModuleName,
    /// An external ID has no module prefix (not in the form `module.name`).
    Unqualified,
}

/// A string that is not a valid `[module.]name` external ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidXmlId {
    value: String,
    problem: XmlIdProblem,
}

impl InvalidXmlId {
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn problem(&self) -> XmlIdProblem {
        self.problem
    }
}

impl fmt::Display for InvalidXmlId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = &self.value;
        match self.problem {
            XmlIdProblem::Empty => write!(f, "an external ID must not be empty"),
            XmlIdProblem::TooManyDots => write!(
                f,
                "The ID reference \"{v}\" must contain maximum one dot. They are used to refer to \
                 other modules ID, in the form: module.record_id"
            ),
            XmlIdProblem::EmptyModule => write!(f, "external ID \"{v}\" has an empty module part"),
            XmlIdProblem::EmptyName => write!(f, "external ID \"{v}\" has an empty name part"),
            XmlIdProblem::Whitespace => write!(f, "external ID \"{v}\" contains whitespace"),
            XmlIdProblem::ControlCharacter => {
                write!(f, "external ID {v:?} contains a control character")
            }
            XmlIdProblem::InvalidModuleName => write!(
                f,
                "external ID \"{v}\": the part before the dot must be a module name \
                 (ASCII letters, digits and '_')"
            ),
            XmlIdProblem::Unqualified => write!(
                f,
                "external ID \"{v}\" has no module: external IDs must be in the form module.name"
            ),
        }
    }
}

impl std::error::Error for InvalidXmlId {}

/// An external ID (XML ID) `[module.]name`, validated by [`XmlId::parse`].
///
/// Rules: not empty; at most one dot; neither part empty; no whitespace or control characters
/// anywhere; the module part (when present) is ASCII letters, digits and `_`.
/// `base.lang_sr@latin`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct XmlId {
    module: Option<String>,
    name: String,
}

impl XmlId {
    /// Parse `name` or `module.name`. The input is used as written (no trimming).
    pub fn parse(value: &str) -> Result<Self, InvalidXmlId> {
        let fail = |problem| InvalidXmlId {
            value: value.to_string(),
            problem,
        };
        if value.is_empty() {
            return Err(fail(XmlIdProblem::Empty));
        }
        if value.chars().any(char::is_whitespace) {
            return Err(fail(XmlIdProblem::Whitespace));
        }
        if value.chars().any(char::is_control) {
            return Err(fail(XmlIdProblem::ControlCharacter));
        }
        match value.split_once('.') {
            None => Ok(Self {
                module: None,
                name: value.to_string(),
            }),
            Some((first, name)) => {
                if name.contains('.') {
                    return Err(fail(XmlIdProblem::TooManyDots));
                }
                if first.is_empty() {
                    return Err(fail(XmlIdProblem::EmptyModule));
                }
                if name.is_empty() {
                    return Err(fail(XmlIdProblem::EmptyName));
                }
                if !first.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Err(fail(XmlIdProblem::InvalidModuleName));
                }
                Ok(Self {
                    module: Some(first.to_string()),
                    name: name.to_string(),
                })
            }
        }
    }

    /// Build `module.name` from its parts, with the same checks as [`XmlId::parse`].
    pub fn qualified(module: &str, name: &str) -> Result<Self, InvalidXmlId> {
        // A dot in `name` is reported as TooManyDots, an empty part as EmptyModule/EmptyName.
        Self::parse(&format!("{module}.{name}"))
    }

    pub fn module(&self) -> Option<&str> {
        self.module.as_deref()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `module.name`, using `default_module` when the ID has no module part.
    pub fn complete_name(&self, default_module: &str) -> String {
        match &self.module {
            Some(m) => format!("{m}.{}", self.name),
            None => format!("{default_module}.{}", self.name),
        }
    }

    /// Converts to [`QualifiedXmlId`] if a module is present, or returns an [`XmlIdProblem::Unqualified`] error.
    pub fn require_qualified(&self) -> Result<QualifiedXmlId, InvalidXmlId> {
        match &self.module {
            Some(m) => Ok(QualifiedXmlId {
                module: m.clone(),
                name: self.name.clone(),
            }),
            None => Err(InvalidXmlId {
                value: self.to_string(),
                problem: XmlIdProblem::Unqualified,
            }),
        }
    }

    /// Converts to [`QualifiedXmlId`] if a module is present.
    pub fn as_qualified(&self) -> Option<QualifiedXmlId> {
        self.require_qualified().ok()
    }

    /// Returns the qualified XML ID `module.name`, using `default_module` when the ID has no module part.
    pub fn complete_xml_id(&self, default_module: &str) -> Result<QualifiedXmlId, InvalidXmlId> {
        match &self.module {
            Some(m) => QualifiedXmlId::new(m, &self.name),
            None => QualifiedXmlId::new(default_module, &self.name),
        }
    }
}

/// An external ID (XML ID) guaranteed to have a module part: `module.name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QualifiedXmlId {
    module: String,
    name: String,
}

impl QualifiedXmlId {
    /// Parse `module.name`. Requires a module part.
    pub fn parse(value: &str) -> Result<Self, InvalidXmlId> {
        let xid = XmlId::parse(value)?;
        xid.require_qualified()
    }

    /// Build `module.name` from its parts, validating that both are valid.
    pub fn new(module: impl Into<String>, name: impl Into<String>) -> Result<Self, InvalidXmlId> {
        let m = module.into();
        let n = name.into();
        let xid = XmlId::qualified(&m, &n)?;
        xid.require_qualified()
    }

    /// The module part.
    pub fn module(&self) -> &str {
        &self.module
    }

    /// The name part.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Convert into an [`XmlId`].
    pub fn to_xml_id(&self) -> XmlId {
        XmlId {
            module: Some(self.module.clone()),
            name: self.name.clone(),
        }
    }
}

impl fmt::Display for QualifiedXmlId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.module, self.name)
    }
}

impl From<QualifiedXmlId> for XmlId {
    fn from(q: QualifiedXmlId) -> Self {
        q.to_xml_id()
    }
}

impl TryFrom<XmlId> for QualifiedXmlId {
    type Error = InvalidXmlId;

    fn try_from(xid: XmlId) -> Result<Self, Self::Error> {
        xid.require_qualified()
    }
}

impl TryFrom<&XmlId> for QualifiedXmlId {
    type Error = InvalidXmlId;

    fn try_from(xid: &XmlId) -> Result<Self, Self::Error> {
        xid.require_qualified()
    }
}

impl fmt::Display for XmlId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.module {
            Some(m) => write!(f, "{m}.{}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

/// The environment an operation runs under, as read from its containers and its own attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpEnv {
    noupdate: bool,
    auto_sequence: bool,
    uid: Option<XmlId>,
    contexts: Vec<Expr>,
}

impl OpEnv {
    /// Effective `noupdate` of the operation (its direct container's, else inherited).
    pub fn noupdate(&self) -> bool {
        self.noupdate
    }

    /// `auto_sequence` flag of the operation's direct container.
    /// numbers the records.
    pub fn auto_sequence(&self) -> bool {
        self.auto_sequence
    }

    /// The user the operation runs as: the innermost `uid` attribute, if any.
    pub fn uid(&self) -> Option<&XmlId> {
        self.uid.as_ref()
    }

    /// Context expressions from the outermost container to the operation itself.
    pub fn context_chain(&self) -> &[Expr] {
        &self.contexts
    }

    pub(crate) fn root(noupdate: bool) -> Self {
        Self {
            noupdate,
            ..Self::default()
        }
    }

    /// Environment of a nested container.
    pub(crate) fn container(
        &self,
        noupdate: bool,
        auto_sequence: bool,
        uid: Option<XmlId>,
        context: Option<Expr>,
    ) -> Self {
        let mut env = self.with_own(uid, context);
        env.noupdate = noupdate;
        env.auto_sequence = auto_sequence;
        env
    }

    /// Environment of an operation with its own `uid`/`context` attributes.
    pub(crate) fn with_own(&self, uid: Option<XmlId>, context: Option<Expr>) -> Self {
        let mut env = self.clone();
        if uid.is_some() {
            env.uid = uid;
        }
        if let Some(ctx) = context {
            env.contexts.push(ctx);
        }
        env
    }
}

/// One operation of a data file, in document order.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum DataOperation {
    Record(RecordOp),
    MenuItem(MenuItemOp),
    Delete(DeleteOp),
    Function(FunctionOp),
    Template(TemplateOp),
    Asset(AssetOp),
    Csv(CsvOp),
}

impl DataOperation {
    pub fn source_file(&self) -> &str {
        match self {
            Self::Record(op) => op.source_file(),
            Self::MenuItem(op) => op.source_file(),
            Self::Delete(op) => op.source_file(),
            Self::Function(op) => op.source_file(),
            Self::Template(op) => op.source_file(),
            Self::Asset(op) => op.source_file(),
            Self::Csv(op) => op.source_file(),
        }
    }

    pub fn line(&self) -> usize {
        match self {
            Self::Record(op) => op.line(),
            Self::MenuItem(op) => op.line(),
            Self::Delete(op) => op.line(),
            Self::Function(op) => op.line(),
            Self::Template(op) => op.line(),
            Self::Asset(op) => op.line(),
            Self::Csv(op) => op.line(),
        }
    }

    pub fn env(&self) -> &OpEnv {
        match self {
            Self::Record(op) => op.env(),
            Self::MenuItem(op) => op.env(),
            Self::Delete(op) => op.env(),
            Self::Function(op) => op.env(),
            Self::Template(op) => op.env(),
            Self::Asset(op) => op.env(),
            Self::Csv(op) => op.env(),
        }
    }
}

/// A CSV data file as one operation: the parsed file and the environment it loads under (the
/// file-level `noupdate`; CSV files have no containers, `uid` or `context`).
#[derive(Debug, Clone, PartialEq)]
pub struct CsvOp {
    pub(crate) import: crate::csv::CsvImport,
    pub(crate) env: OpEnv,
}

impl CsvOp {
    /// The parsed file.
    pub fn import(&self) -> &crate::csv::CsvImport {
        &self.import
    }

    /// The environment: the file-level `noupdate` only.
    pub fn env(&self) -> &OpEnv {
        &self.env
    }

    /// The file name as given to the reader.
    pub fn source_file(&self) -> &str {
        self.import.source_file()
    }

    /// Always 1: the operation is the whole file (rows carry their own lines).
    pub fn line(&self) -> usize {
        1
    }
}

/// `<record>` (and a CSV row).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordOp {
    pub(crate) id: Option<XmlId>,
    pub(crate) model: String,
    pub(crate) forcecreate: Option<bool>,
    pub(crate) fields: Vec<FieldOp>,
    pub(crate) translations: Vec<FieldOp>,
    pub(crate) env: OpEnv,
    pub(crate) file: Arc<str>,
    pub(crate) line: usize,
    pub(crate) origin: Option<String>,
}

impl RecordOp {
    pub fn id(&self) -> Option<&XmlId> {
        self.id.as_ref()
    }

    /// The element this record was built from when it is not a `<record>` (a `<template>`).
    pub fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn forcecreate(&self) -> Option<bool> {
        self.forcecreate
    }

    /// Field values in document order.
    pub fn fields(&self) -> &[FieldOp] {
        &self.fields
    }

    /// Translation field values in document order (`name@fr_FR` or `<field lang="fr_FR">`).
    pub fn translations(&self) -> &[FieldOp] {
        &self.translations
    }

    /// The first translation field for `name` and `lang`.
    pub fn translation(&self, name: &str, lang: &str) -> Option<&FieldOp> {
        self.translations
            .iter()
            .find(|f| f.name == name && f.lang() == Some(lang))
    }

    /// The first field named `name`.
    pub fn field(&self, name: &str) -> Option<&FieldOp> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn env(&self) -> &OpEnv {
        &self.env
    }

    pub fn source_file(&self) -> &str {
        &self.file
    }

    pub fn line(&self) -> usize {
        self.line
    }
}

/// `<field name="…">` of a record or asset.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldOp {
    pub(crate) name: String,
    pub(crate) value: FieldValue,
    pub(crate) line: usize,
    pub(crate) lang: Option<String>,
}

impl FieldOp {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The language code of a translation field value, if specified (`name@fr_FR` or `<field lang="fr_FR">`).
    pub fn lang(&self) -> Option<&str> {
        self.lang.as_deref()
    }

    pub fn value(&self) -> &FieldValue {
        &self.value
    }

    pub fn line(&self) -> usize {
        self.line
    }
}

/// The value form of a `<field>`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum FieldValue {
    /// Text content (type `char`, the default), entities resolved, line ends normalised.
    Text(String),
    /// `ref="xid"`.
    Ref(XmlId),
    /// `eval="expr"`; `model` sets the `obj` of the eval context.
    Eval {
        expr: Expr,
        model: Option<String>,
    },
    /// `search="domain"` with optional `model` and `use` (default `id`).
    Search {
        domain: Expr,
        model: Option<String>,
        use_field: Option<String>,
    },
    /// `type="file"`: a path in the module, stored as `"<module>,<path>"`.
    ModulePath(String),
    /// `file="path"` with type `char` (or no type): the file's text content.
    FileText(String),
    /// `type="bytes" file="path"`: the file's binary content.
    FileBytes(String),
    FileBase64(String),
    /// `type="int"`; `None` for the literal `None`.
    Int(Option<i64>),
    /// `type="float"`.
    Float(f64),
    List(Vec<ValueOp>),
    Tuple(Vec<ValueOp>),
    /// `type="xml"`: `<?xml version="1.0"?>\n` plus the child markup, several roots wrapped in
    /// `<data>`. `%(xid)d` markers are kept for.
    Xml(String),
    /// `type="html"`: the children serialised the way lxml's `method="html"` does.
    Html(String),
    /// Nested `<record>` elements of a one2many field, created after the parent.
    SubRecords(Vec<RecordOp>),
    /// A CSV cell for `load()` coercion (`None` for an empty cell).
    CsvCell(Option<String>),
}

/// A `<value>` of a list/tuple field or a `<function>` argument.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ValueOp {
    Text(String),
    Eval {
        expr: Expr,
        model: Option<String>,
    },
    Search {
        domain: Expr,
        model: Option<String>,
        use_field: Option<String>,
    },
    ModulePath(String),
    FileText(String),
    FileBytes(String),
    FileBase64(String),
    Int(Option<i64>),
    Float(f64),
    /// `type="list"`: the schema allows no child elements in `<value>`, so the list is empty.
    EmptyList,
    /// `type="tuple"`, empty for the same reason.
    EmptyTuple,
}

/// A `groups` entry: `group` links, `-group` unlinks (menus only).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GroupCommand {
    Link(XmlId),
    Unlink(XmlId),
}

impl GroupCommand {
    pub fn xml_id(&self) -> &XmlId {
        match self {
            Self::Link(x) | Self::Unlink(x) => x,
        }
    }
}

/// `<menuitem>`.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItemOp {
    pub(crate) id: XmlId,
    pub(crate) name: Option<String>,
    pub(crate) parent: Option<XmlId>,
    pub(crate) sequence: Option<i32>,
    pub(crate) action: Option<XmlId>,
    pub(crate) web_icon: Option<String>,
    pub(crate) groups: Vec<GroupCommand>,
    pub(crate) active: Option<bool>,
    pub(crate) children: Vec<MenuItemOp>,
    pub(crate) env: OpEnv,
    pub(crate) file: Arc<str>,
    pub(crate) line: usize,
}

impl MenuItemOp {
    pub fn id(&self) -> &XmlId {
        &self.id
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The enclosing `<menuitem>` for nested menus, else the `parent` attribute.
    pub fn parent(&self) -> Option<&XmlId> {
        self.parent.as_ref()
    }

    pub fn sequence(&self) -> Option<i32> {
        self.sequence
    }

    pub fn action(&self) -> Option<&XmlId> {
        self.action.as_ref()
    }

    /// Only top-level menus without `parent` carry an icon (the schema refuses it elsewhere).
    pub fn web_icon(&self) -> Option<&str> {
        self.web_icon.as_deref()
    }

    pub fn groups(&self) -> &[GroupCommand] {
        &self.groups
    }

    /// `None` when absent or empty: active by default.
    pub fn active(&self) -> Option<bool> {
        self.active
    }

    pub fn children(&self) -> &[MenuItemOp] {
        &self.children
    }

    pub fn env(&self) -> &OpEnv {
        &self.env
    }

    pub fn source_file(&self) -> &str {
        &self.file
    }

    pub fn line(&self) -> usize {
        self.line
    }
}

/// What a `<delete>` removes; the schema allows exactly one of `id` and `search`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeleteTarget {
    Id(XmlId),
    Search(Expr),
}

/// `<delete>`; runs in update mode even under `noupdate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOp {
    pub(crate) model: String,
    pub(crate) target: DeleteTarget,
    pub(crate) env: OpEnv,
    pub(crate) file: Arc<str>,
    pub(crate) line: usize,
}

impl DeleteOp {
    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn target(&self) -> &DeleteTarget {
        &self.target
    }

    pub fn env(&self) -> &OpEnv {
        &self.env
    }

    pub fn source_file(&self) -> &str {
        &self.file
    }

    pub fn line(&self) -> usize {
        self.line
    }
}

/// A positional argument of a `<function>`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum FunctionArg {
    Value(ValueOp),
    Function(Box<FunctionOp>),
}

/// `<function>`.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionOp {
    pub(crate) model: String,
    pub(crate) name: String,
    pub(crate) eval: Option<Expr>,
    pub(crate) args: Vec<FunctionArg>,
    pub(crate) kwargs: Vec<(String, ValueOp)>,
    pub(crate) env: OpEnv,
    pub(crate) file: Arc<str>,
    pub(crate) line: usize,
}

impl FunctionOp {
    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `eval` attribute: the leading positional arguments (a list expression).
    pub fn eval(&self) -> Option<&Expr> {
        self.eval.as_ref()
    }

    /// `<value>` without `name` and nested `<function>` children, in order.
    pub fn args(&self) -> &[FunctionArg] {
        &self.args
    }

    /// `<value name="…">` children, in order; names are unique.
    pub fn kwargs(&self) -> &[(String, ValueOp)] {
        &self.kwargs
    }

    pub fn kwarg(&self, key: &str) -> Option<&ValueOp> {
        self.kwargs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The environment; a nested `<function>` runs in its parent's.
    pub fn env(&self) -> &OpEnv {
        &self.env
    }

    pub fn source_file(&self) -> &str {
        &self.file
    }

    pub fn line(&self) -> usize {
        self.line
    }
}

/// Target model family of the `<template>`/`<asset>` shorthands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShorthandTarget {
    Standard,
    Theme,
}

/// Whether a root `active="True|False"` of a template/asset is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RootActive {
    Apply,
    Skip,
}

/// `<template>`.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateOp {
    pub(crate) id: XmlId,
    pub(crate) full_id: String,
    pub(crate) t_name: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) forcecreate: Option<bool>,
    pub(crate) priority: Option<i32>,
    pub(crate) key: Option<String>,
    pub(crate) website_id: Option<XmlId>,
    pub(crate) technical_usage: Option<String>,
    pub(crate) track: Option<bool>,
    pub(crate) inherit_id: Option<XmlId>,
    pub(crate) primary: bool,
    pub(crate) groups: Vec<GroupCommand>,
    pub(crate) active: Option<bool>,
    pub(crate) customize_show: Option<bool>,
    pub(crate) arch: String,
    pub(crate) env: OpEnv,
    pub(crate) file: Arc<str>,
    pub(crate) line: usize,
}

impl TemplateOp {
    /// `id`, else `t-name`.
    pub fn id(&self) -> &XmlId {
        &self.id
    }

    /// The module-qualified ID (`<module>.<id>` when the ID has no module).
    pub fn full_id(&self) -> &str {
        &self.full_id
    }

    pub fn t_name(&self) -> Option<&str> {
        self.t_name.as_deref()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn forcecreate(&self) -> Option<bool> {
        self.forcecreate
    }

    pub fn priority(&self) -> Option<i32> {
        self.priority
    }

    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    pub fn website_id(&self) -> Option<&XmlId> {
        self.website_id.as_ref()
    }

    pub fn technical_usage(&self) -> Option<&str> {
        self.technical_usage.as_deref()
    }

    pub fn track(&self) -> Option<bool> {
        self.track
    }

    pub fn inherit_id(&self) -> Option<&XmlId> {
        self.inherit_id.as_ref()
    }

    /// `primary="True"` (only with `inherit_id`).
    pub fn primary(&self) -> bool {
        self.primary
    }

    pub fn groups(&self) -> &[GroupCommand] {
        &self.groups
    }

    pub fn active(&self) -> Option<bool> {
        self.active
    }

    pub fn customize_show(&self) -> Option<bool> {
        self.customize_show
    }

    pub fn arch(&self) -> &str {
        &self.arch
    }

    pub fn env(&self) -> &OpEnv {
        &self.env
    }

    pub fn source_file(&self) -> &str {
        &self.file
    }

    pub fn line(&self) -> usize {
        self.line
    }

    pub fn to_record_op(&self, target: ShorthandTarget, root_active: RootActive) -> RecordOp {
        let model = match target {
            ShorthandTarget::Standard => "ir.ui.view",
            ShorthandTarget::Theme => "theme.ir.ui.view",
        };
        let line = self.line;
        let mut fields = Vec::new();
        let mut push = |name: &str, value: FieldValue| {
            fields.push(FieldOp {
                name: name.to_string(),
                value,
                line,
                lang: None,
            })
        };
        push(
            "name",
            FieldValue::Text(self.name.clone().unwrap_or_else(|| self.id.to_string())),
        );
        push(
            "key",
            FieldValue::Text(self.key.clone().unwrap_or_else(|| self.full_id.clone())),
        );
        push("type", FieldValue::Text("qweb".to_string()));
        if let Some(track) = self.track {
            push(
                "track",
                FieldValue::Eval {
                    expr: Expr::new(py_bool(track)),
                    model: None,
                },
            );
        }
        if let Some(priority) = self.priority {
            push("priority", FieldValue::Int(Some(i64::from(priority))));
        }
        if let Some(inherit_id) = &self.inherit_id {
            push("inherit_id", FieldValue::Ref(inherit_id.clone()));
        }
        if let Some(usage) = &self.technical_usage {
            push("technical_usage", FieldValue::Text(usage.clone()));
        }
        if let Some(website) = &self.website_id {
            push("website_id", FieldValue::Ref(website.clone()));
        }
        if let (Some(active), RootActive::Apply) = (self.active, root_active) {
            push(
                "active",
                FieldValue::Eval {
                    expr: Expr::new(py_bool(active)),
                    model: None,
                },
            );
        }
        if let Some(show) = self.customize_show {
            push(
                "customize_show",
                FieldValue::Eval {
                    expr: Expr::new(py_bool(show)),
                    model: None,
                },
            );
        }
        if !self.groups.is_empty() {
            let refs: Vec<String> = self
                .groups
                .iter()
                .map(|g| format!("ref('{}')", g.xml_id()))
                .collect();
            push(
                "group_ids",
                FieldValue::Eval {
                    expr: Expr::new(format!("[Command.set([{}])]", refs.join(", "))),
                    model: None,
                },
            );
        }
        if self.primary {
            push("mode", FieldValue::Text("primary".to_string()));
        }
        push(
            "arch",
            FieldValue::Xml(format!("<?xml version=\"1.0\"?>\n{}", self.arch)),
        );
        RecordOp {
            id: Some(self.id.clone()),
            model: model.to_string(),
            forcecreate: self.forcecreate,
            fields,
            translations: Vec::new(),
            env: self.env.clone(),
            file: Arc::clone(&self.file),
            line,
            origin: Some(format!("<template id=\"{}\">", self.id)),
        }
    }
}

/// `<asset>`.
#[derive(Debug, Clone, PartialEq)]
pub struct AssetOp {
    pub(crate) id: XmlId,
    pub(crate) name: String,
    pub(crate) active: Option<bool>,
    pub(crate) bundle: String,
    pub(crate) directive: Option<String>,
    pub(crate) path: String,
    pub(crate) fields: Vec<FieldOp>,
    pub(crate) env: OpEnv,
    pub(crate) file: Arc<str>,
    pub(crate) line: usize,
}

impl AssetOp {
    pub fn id(&self) -> &XmlId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn active(&self) -> Option<bool> {
        self.active
    }

    /// `<bundle>` text as written.
    pub fn bundle(&self) -> &str {
        &self.bundle
    }

    pub fn directive(&self) -> Option<&str> {
        self.directive.as_deref()
    }

    /// `<path>` text as written.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn fields(&self) -> &[FieldOp] {
        &self.fields
    }

    pub fn env(&self) -> &OpEnv {
        &self.env
    }

    pub fn source_file(&self) -> &str {
        &self.file
    }

    pub fn line(&self) -> usize {
        self.line
    }

    pub fn to_record_op(&self, target: ShorthandTarget, root_active: RootActive) -> RecordOp {
        let model = match target {
            ShorthandTarget::Standard => "ir.asset",
            ShorthandTarget::Theme => "theme.ir.asset",
        };
        let line = self.line;
        let field = |name: &str, value: FieldValue| FieldOp {
            name: name.to_string(),
            value,
            line,
            lang: None,
        };
        let mut fields = vec![
            field("name", FieldValue::Text(self.name.clone())),
            field("bundle", FieldValue::Text(self.bundle.clone())),
        ];
        if let Some(directive) = &self.directive {
            fields.push(field("directive", FieldValue::Text(directive.clone())));
        }
        fields.push(field("path", FieldValue::Text(self.path.clone())));
        if let (Some(active), RootActive::Apply) = (self.active, root_active) {
            fields.push(field(
                "active",
                FieldValue::Eval {
                    expr: Expr::new(py_bool(active)),
                    model: None,
                },
            ));
        }
        fields.extend(self.fields.iter().cloned());
        RecordOp {
            id: Some(self.id.clone()),
            model: model.to_string(),
            forcecreate: None,
            fields,
            translations: Vec::new(),
            env: self.env.clone(),
            file: Arc::clone(&self.file),
            line,
            origin: Some(format!("<asset id=\"{}\">", self.id)),
        }
    }
}

fn py_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}
