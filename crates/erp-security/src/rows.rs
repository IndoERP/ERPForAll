//! Policy rows: what the loader reads from the policy tables and the compiler turns
//! into a policy snapshot (seam 3; the loader column contract is in
//! ).
//!
//! The loader reads every table in **one** `REPEATABLE READ` snapshot, so the rows are consistent
//! with each other and with the `policy` generation read in the same snapshot. They are plain
//! values: no database handle, no registry. Each row type validates its own fields when it is
//! built ([`RuleRow::new`], [`GroupRow::new`], …) and [`PolicyRowsBuilder::build`] validates the
//! set as a whole: unique ids and XML ids, references between rows (a rule's group, an
//! implication's groups, a group's privilege, the groups of a field) and the size limits below.
//! A violation is a loader bug or a damaged database, never something to skip: the build fails
//! with a [`RowError`].
//!
//! What the rows deliberately do **not** check: whether a rule's model exists in the registry and
//! whether its domain compiles. Those are per-rule problems the compiler reports in the snapshot's
//! build report (they deny the rule's model and operation, they do not fail the whole build).
//!
//! ```
//! use erp_orm::GroupId;
//! use erp_security::rows::{GroupRow, PolicyRows, RuleId, RuleRow};
//!
//! let user = GroupId::new(1)?;
//! let rows = PolicyRows::builder(7)
//!     .group(GroupRow::new(user, "Internal User")?)
//!     .rule(
//!         RuleRow::new(RuleId::new(10)?, "own partners", "res.partner", "cru")?
//!             .with_group(user)
//!             .with_domain("[('user_id', '=', user.id)]"),
//!     )
//!     .build()?;
//! assert_eq!(rows.generation(), 7);
//! assert_eq!(rows.rules()[0].operation(), "cru");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroI64;

use crate::company_access::GeneratedRow;
use erp_fields::{Error, ErrorKind};
use erp_orm::{GroupId, Operation, OperationSet, PrincipalError, QualifiedXmlId};

/// Most `ir.access` rows a policy may hold.
pub const MAX_RULES: usize = 100_000;
/// Most groups a policy may hold. The group graph keeps one closure bit set per group, so its
/// memory grows with the square of this number (12.5 MB per direction at the limit).
pub const MAX_GROUPS: usize = 10_000;
/// Most group implications (`res_groups_implied_rel` rows).
pub const MAX_IMPLICATIONS: usize = 1_000_000;
/// Most privileges (`res.groups.privilege` rows).
pub const MAX_PRIVILEGES: usize = 10_000;
/// Most capability mapping rows (`ir_capability`).
pub const MAX_CAPABILITIES: usize = 10_000;
/// Most field-group overlay rows (custom `x_` fields with groups).
pub const MAX_FIELD_GROUPS: usize = 100_000;
/// Longest name (rule, group, privilege), group spec or model/field name, in bytes. Names are
/// display text that ends up in error messages, so they are bounded.
pub const MAX_TEXT_BYTES: usize = 4096;
/// Most bytes of text (names, domains, group specs) in all rows together. Bounds the memory one
/// snapshot build copies; a single domain longer than the expression source limit (64 KiB,
/// ) is a rule error of the compiler, not a row error.
pub const MAX_TOTAL_TEXT_BYTES: usize = 64 * 1024 * 1024;

/// Which kind of row a [`RowError`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RowKind {
    /// An `ir.access` row.
    Rule,
    /// A `res.groups` row.
    Group,
    /// A `res_groups_implied_rel` row.
    Implication,
    /// A `res.groups.privilege` row.
    Privilege,
    /// An `ir_capability` row.
    Capability,
    /// A field-groups overlay row.
    FieldGroups,
}

impl RowKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "ir.access",
            Self::Group => "res.groups",
            Self::Implication => "res_groups_implied_rel",
            Self::Privilege => "res.groups.privilege",
            Self::Capability => "ir_capability",
            Self::FieldGroups => "ir_model_fields_group_rel",
        }
    }
}

impl fmt::Display for RowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A policy row or row set that cannot be built.
///
/// Converts into [`erp_fields::Error`] with the kind of [`RowError::kind`] and the same message,
/// with this error as its source.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RowError {
    /// An id outside its range (ids of database rows are positive).
    InvalidId {
        /// Which row kind the id belongs to.
        what: RowKind,
        /// The refused value.
        value: i64,
    },
    /// An `ir.access` operation that is not one of the 15 non-empty subsets of `crud`.
    InvalidOperation {
        /// The rule.
        rule: RuleId,
        /// The refused value (at most 64 characters of it).
        value: String,
    },
    /// A name, model or field name that is empty, too long or has forbidden characters.
    InvalidText {
        /// Which row kind.
        what: RowKind,
        /// The row (its id or name, at most 64 characters).
        row: String,
        /// Which column.
        column: &'static str,
        /// What is wrong, as a predicate ("is empty").
        problem: String,
    },
    InvalidApiKeyDuration {
        /// The group.
        group: GroupId,
        /// The refused value.
        value: f64,
    },
    /// Two rows with one id (or one name, for capabilities; one model and field, for field
    /// groups).
    Duplicate {
        /// Which row kind.
        what: RowKind,
        /// The repeated key.
        key: String,
    },
    /// One XML id names two rows (an external id names exactly one record).
    DuplicateXmlId {
        /// The XML id.
        xmlid: QualifiedXmlId,
    },
    /// A row refers to a group that is not in the rows.
    UnknownGroup {
        /// Which row kind refers to it.
        referrer: RowKind,
        /// The referring row.
        row: String,
        /// The missing group.
        group: GroupId,
    },
    /// A group refers to a privilege that is not in the rows.
    UnknownPrivilege {
        /// The group.
        group: GroupId,
        /// The missing privilege.
        privilege: PrivilegeId,
    },
    /// More rows of one kind than the limit allows.
    TooMany {
        /// Which row kind.
        what: RowKind,
        /// How many there are.
        count: usize,
        /// The limit.
        max: usize,
    },
    /// More text in all rows together than [`MAX_TOTAL_TEXT_BYTES`].
    TooMuchText {
        /// The bytes counted when the limit was passed.
        bytes: usize,
        /// The limit.
        max: usize,
    },
}

impl RowError {
    /// The [`ErrorKind`] this error converts to: `ResourceExhausted` for the size limits,
    /// `Programming` for everything else (the loader read inconsistent or invalid rows).
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::TooMany { .. } | Self::TooMuchText { .. } => ErrorKind::ResourceExhausted,
            Self::InvalidId { .. }
            | Self::InvalidOperation { .. }
            | Self::InvalidText { .. }
            | Self::InvalidApiKeyDuration { .. }
            | Self::Duplicate { .. }
            | Self::DuplicateXmlId { .. }
            | Self::UnknownGroup { .. }
            | Self::UnknownPrivilege { .. } => ErrorKind::Programming,
        }
    }
}

impl fmt::Display for RowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId { what, value } => {
                write!(
                    f,
                    "policy rows: invalid {what} id {value}: ids are positive"
                )
            }
            Self::InvalidOperation { rule, value } => write!(
                f,
                "policy rows: ir.access {rule} has the invalid operation '{value}': expected a \
                 non-empty subset of 'crud' written in c, r, u, d order"
            ),
            Self::InvalidText {
                what,
                row,
                column,
                problem,
            } => write!(f, "policy rows: {what} {row}: {column} {problem}"),
            Self::InvalidApiKeyDuration { group, value } => write!(
                f,
                "policy rows: res.groups {group} has the API key duration {value}: it must be a \
                 finite number of days, at least 0"
            ),
            Self::Duplicate { what, key } => {
                write!(f, "policy rows: {what} {key} is listed twice")
            }
            Self::DuplicateXmlId { xmlid } => {
                write!(f, "policy rows: the XML id {xmlid} names two rows")
            }
            Self::UnknownGroup {
                referrer,
                row,
                group,
            } => write!(
                f,
                "policy rows: {referrer} {row} refers to the group {group}, which is not in the \
                 rows"
            ),
            Self::UnknownPrivilege { group, privilege } => write!(
                f,
                "policy rows: res.groups {group} refers to the privilege {privilege}, which is \
                 not in the rows"
            ),
            Self::TooMany { what, count, max } => write!(
                f,
                "policy rows: {count} {what} rows are more than the {max} a policy may hold"
            ),
            Self::TooMuchText { bytes, max } => write!(
                f,
                "policy rows: more than {max} bytes of names, domains and group specs (at least \
                 {bytes})"
            ),
        }
    }
}

impl std::error::Error for RowError {}

impl From<RowError> for Error {
    fn from(error: RowError) -> Self {
        Error::new(error.kind(), error.to_string()).with_source(error)
    }
}

/// At most 64 characters of an input echoed in an error message.
pub(crate) fn clip(value: &str) -> String {
    const MAX: usize = 64;
    match value.char_indices().nth(MAX) {
        Some((end, _)) => format!("{}…", &value[..end]),
        None => value.to_owned(),
    }
}

macro_rules! row_id {
    ($(#[$doc:meta])* $name:ident, $kind:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(NonZeroI64);

        impl $name {
            /// The id of a database row.
            ///
            /// # Errors
            ///
            /// [`RowError::InvalidId`] unless `id` is positive.
            pub fn new(id: i64) -> Result<Self, RowError> {
                match NonZeroI64::new(id) {
                    Some(n) if id > 0 => Ok(Self(n)),
                    // zero or negative
                    _ => Err(RowError::InvalidId {
                        what: RowKind::$kind,
                        value: id,
                    }),
                }
            }

            /// The raw id.
            pub fn get(self) -> i64 {
                self.0.get()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

row_id!(
    /// An `ir.access` id (positive).
    RuleId,
    Rule
);
row_id!(
    /// A `res.groups.privilege` id (positive).
    PrivilegeId,
    Privilege
);

/// A group id of a row, refused like [`RowError::InvalidId`] when it is not positive.
///
/// # Errors
///
/// [`RowError::InvalidId`] (with [`RowKind::Group`]) unless `id` is positive.
pub fn group_id(id: i64) -> Result<GroupId, RowError> {
    GroupId::new(id).map_err(|_: PrincipalError| RowError::InvalidId {
        what: RowKind::Group,
        value: id,
    })
}

fn check_text(
    what: RowKind,
    row: impl FnOnce() -> String,
    column: &'static str,
    value: &str,
    allow_empty: bool,
) -> Result<Box<str>, RowError> {
    let problem = if value.len() > MAX_TEXT_BYTES {
        Some(format!(
            "is {} bytes, more than the {MAX_TEXT_BYTES} allowed",
            value.len()
        ))
    } else if !allow_empty && value.is_empty() {
        Some("is empty".to_owned())
    } else {
        None
    };
    match problem {
        Some(problem) => Err(RowError::InvalidText {
            what,
            row: clip(&row()),
            column,
            problem,
        }),
        None => Ok(value.into()),
    }
}

/// A model or field name: non-empty, at most [`MAX_TEXT_BYTES`], no whitespace or control
/// characters (the registry's names never have them).
fn check_identifier(
    what: RowKind,
    row: impl Fn() -> String,
    column: &'static str,
    value: &str,
) -> Result<Box<str>, RowError> {
    let checked = check_text(what, &row, column, value, false)?;
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(RowError::InvalidText {
            what,
            row: clip(&row()),
            column,
            problem: format!("'{}' has whitespace or control characters", clip(value)),
        });
    }
    Ok(checked)
}

fn parse_operation(rule: RuleId, value: &str) -> Result<OperationSet, RowError> {
    let invalid = || RowError::InvalidOperation {
        rule,
        value: clip(value),
    };
    let mut ops = Vec::with_capacity(4);
    let mut last: Option<usize> = None;
    for c in value.chars() {
        let (rank, op) = match c {
            'c' => (0, Operation::Create),
            'r' => (1, Operation::Read),
            'u' => (2, Operation::Write),
            'd' => (3, Operation::Unlink),
            _ => return Err(invalid()),
        };
        if last.is_some_and(|l| rank <= l) {
            return Err(invalid());
        }
        last = Some(rank);
        ops.push(op);
    }
    if ops.is_empty() {
        return Err(invalid());
    }
    Ok(OperationSet::of(ops))
}

/// The `crud` spelling of an operation set (`c, r, u, d` order); empty for the empty set.
pub fn operation_string(ops: OperationSet) -> String {
    [
        (Operation::Create, 'c'),
        (Operation::Read, 'r'),
        (Operation::Write, 'u'),
        (Operation::Unlink, 'd'),
    ]
    .into_iter()
    .filter(|(op, _)| ops.contains(*op))
    .map(|(_, c)| c)
    .collect()
}

pub fn covers(operation: OperationSet, op: Operation) -> bool {
    operation.contains(op)
}

pub fn operation_from_flags(checked: OperationSet) -> OperationSet {
    if checked.is_empty() {
        OperationSet::of([Operation::Read])
    } else {
        checked
    }
}

pub fn operation_at_create(checked: OperationSet) -> Option<OperationSet> {
    if checked.is_empty() {
        None
    } else {
        Some(checked)
    }
}

pub const NON_STANDARD_PREFIXES: [&str; 3] = ["__export__", "__custom__", "studio_customization"];

/// One active `ir.access` row: a **permission** when it has a group, a **restriction** when it
/// has none.
///
/// The model is the `ir.model` name the row's `model_id` points to; the
/// domain is the stored text, unparsed (an empty or blank domain means every record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleRow {
    id: RuleId,
    xmlid: Option<QualifiedXmlId>,
    name: Box<str>,
    model: Box<str>,
    group: Option<GroupId>,
    ops: OperationSet,
    domain: Option<Box<str>>,
    generated: bool,
}

impl RuleRow {
    /// A rule without group (a restriction) and without domain.
    ///
    /// # Errors
    ///
    /// [`RowError::InvalidText`] for a name longer than [`MAX_TEXT_BYTES`] or an empty, too long
    /// or malformed model name; [`RowError::InvalidOperation`] for an operation outside the 15
    /// `crud` subsets.
    pub fn new(id: RuleId, name: &str, model: &str, operation: &str) -> Result<Self, RowError> {
        let label = || id.to_string();
        let name = check_text(RowKind::Rule, label, "name", name, true)?;
        let model = check_identifier(RowKind::Rule, label, "model", model)?;
        let ops = parse_operation(id, operation)?;
        Ok(Self {
            id,
            xmlid: None,
            name,
            model,
            group: None,
            ops,
            domain: None,
            generated: false,
        })
    }

    /// Mark the row as a generated company restriction (`ir_access.generated`, design
    /// ).
    #[must_use]
    pub fn with_generated(mut self) -> Self {
        self.generated = true;
        self
    }

    /// Whether the row is a generated company restriction.
    pub fn generated(&self) -> bool {
        self.generated
    }

    /// Make the rule a permission of `group`.
    #[must_use]
    pub fn with_group(mut self, group: GroupId) -> Self {
        self.group = Some(group);
        self
    }

    /// Set the rule's external id (`ir_model_data`; the oldest one when it has several).
    #[must_use]
    pub fn with_xmlid(mut self, xmlid: QualifiedXmlId) -> Self {
        self.xmlid = Some(xmlid);
        self
    }

    /// Set the domain text (Python-literal list syntax with the names of
    /// ). Its size is checked by the compiler (a rule error),
    /// and in total by [`PolicyRowsBuilder::build`].
    #[must_use]
    pub fn with_domain(mut self, domain: &str) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// The id.
    pub fn id(&self) -> RuleId {
        self.id
    }

    /// The external id, if any.
    pub fn xmlid(&self) -> Option<&QualifiedXmlId> {
        self.xmlid.as_ref()
    }

    /// The label.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The group of a permission; `None` for a restriction.
    pub fn group(&self) -> Option<GroupId> {
        self.group
    }

    /// The operations the rule applies to.
    pub fn ops(&self) -> OperationSet {
        self.ops
    }

    pub fn operation(&self) -> String {
        operation_string(self.ops)
    }

    /// The domain text as stored, if any.
    pub fn domain(&self) -> Option<&str> {
        self.domain.as_deref()
    }

    /// Whether the rule comes from a module: it has an external id that is not an export or a
    /// customization ([`NON_STANDARD_PREFIXES`]). Customizing a standard rule copies it and
    /// archives the original (`customize()`, on the `ir.access` model).
    pub fn is_standard(&self) -> bool {
        self.xmlid.as_ref().is_some_and(|x| {
            !NON_STANDARD_PREFIXES
                .iter()
                .any(|p| x.as_str().starts_with(p))
        })
    }

    fn text_bytes(&self) -> usize {
        self.name.len() + self.model.len() + self.domain.as_deref().map_or(0, str::len)
    }
}

/// One `res.groups` row.
///
/// `sequence` and `api_key_duration` are `None` when the column is `NULL` (no default is put in
/// their place). The XML ids are every `ir_model_data` id of the group, so a lookup by any of them
/// finds it.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupRow {
    id: GroupId,
    xmlids: Vec<QualifiedXmlId>,
    name: Box<str>,
    privilege: Option<PrivilegeId>,
    sequence: Option<i32>,
    share: bool,
    api_key_duration: Option<f64>,
}

impl GroupRow {
    /// A group without XML id, privilege or sequence, not a share group.
    ///
    /// # Errors
    ///
    /// [`RowError::InvalidText`] for a name longer than [`MAX_TEXT_BYTES`].
    pub fn new(id: GroupId, name: &str) -> Result<Self, RowError> {
        let name = check_text(RowKind::Group, || id.to_string(), "name", name, true)?;
        Ok(Self {
            id,
            xmlids: Vec::new(),
            name,
            privilege: None,
            sequence: None,
            share: false,
            api_key_duration: None,
        })
    }

    /// Add an external id (repeats are refused by [`PolicyRowsBuilder::build`]).
    #[must_use]
    pub fn with_xmlid(mut self, xmlid: QualifiedXmlId) -> Self {
        self.xmlids.push(xmlid);
        self
    }

    /// Set the privilege (`privilege_id`).
    #[must_use]
    pub fn with_privilege(mut self, privilege: PrivilegeId) -> Self {
        self.privilege = Some(privilege);
        self
    }

    /// Set the sequence.
    #[must_use]
    pub fn with_sequence(mut self, sequence: i32) -> Self {
        self.sequence = Some(sequence);
        self
    }

    /// Mark the group as a share group (portal or public user type).
    #[must_use]
    pub fn with_share(mut self, share: bool) -> Self {
        self.share = share;
        self
    }

    /// Set the maximum API key duration in days.
    ///
    /// # Errors
    ///
    /// [`RowError::InvalidApiKeyDuration`] for a negative or non-finite value.
    pub fn with_api_key_duration(mut self, days: f64) -> Result<Self, RowError> {
        if !days.is_finite() || days < 0.0 {
            return Err(RowError::InvalidApiKeyDuration {
                group: self.id,
                value: days,
            });
        }
        self.api_key_duration = Some(days);
        Ok(self)
    }

    /// The id.
    pub fn id(&self) -> GroupId {
        self.id
    }

    /// The external ids (sorted by [`PolicyRowsBuilder::build`]).
    pub fn xmlids(&self) -> &[QualifiedXmlId] {
        &self.xmlids
    }

    /// The name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The privilege, if any.
    pub fn privilege(&self) -> Option<PrivilegeId> {
        self.privilege
    }

    /// The sequence, `None` when `NULL`.
    pub fn sequence(&self) -> Option<i32> {
        self.sequence
    }

    /// Whether this is a share group.
    pub fn share(&self) -> bool {
        self.share
    }

    /// The maximum API key duration in days, `None` when `NULL`.
    pub fn api_key_duration(&self) -> Option<f64> {
        self.api_key_duration
    }
}

/// One `res_groups_implied_rel` row: members of `group` are also members of `implies`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Implication {
    group: GroupId,
    implies: GroupId,
}

impl Implication {
    /// `group` implies `implies` (a group may imply itself; the closure is reflexive anyway).
    pub fn new(group: GroupId, implies: GroupId) -> Self {
        Self { group, implies }
    }

    /// The implying group (`gid`).
    pub fn group(&self) -> GroupId {
        self.group
    }

    /// The implied group (`hid`).
    pub fn implies(&self) -> GroupId {
        self.implies
    }
}

/// One `res.groups.privilege` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegeRow {
    id: PrivilegeId,
    name: Box<str>,
    sequence: Option<i32>,
}

impl PrivilegeRow {
    /// A privilege without sequence.
    ///
    /// # Errors
    ///
    /// [`RowError::InvalidText`] for a name longer than [`MAX_TEXT_BYTES`].
    pub fn new(id: PrivilegeId, name: &str) -> Result<Self, RowError> {
        let name = check_text(RowKind::Privilege, || id.to_string(), "name", name, true)?;
        Ok(Self {
            id,
            name,
            sequence: None,
        })
    }

    /// Set the sequence.
    #[must_use]
    pub fn with_sequence(mut self, sequence: i32) -> Self {
        self.sequence = Some(sequence);
        self
    }

    /// The id.
    pub fn id(&self) -> PrivilegeId {
        self.id
    }

    /// The name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The sequence, `None` when `NULL`.
    pub fn sequence(&self) -> Option<i32> {
        self.sequence
    }
}

/// One `ir_capability` row: the group spec that holds a named capability.
///
/// The spec is kept as text; parses group specs. The name follows the rules of
/// `erp_orm::capability!` names: dot-separated parts of lowercase ASCII letters, digits and `_`,
/// at most 64 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRow {
    name: Box<str>,
    groups: Box<str>,
}

impl CapabilityRow {
    /// The mapping of capability `name` to the group spec `groups`.
    ///
    /// # Errors
    ///
    /// [`RowError::InvalidText`] for a malformed name or a spec that is empty or longer than
    /// [`MAX_TEXT_BYTES`].
    pub fn new(name: &str, groups: &str) -> Result<Self, RowError> {
        let valid = !name.is_empty()
            && name.len() <= erp_orm::MAX_CAPABILITY_LEN
            && name.split('.').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            });
        if !valid {
            return Err(RowError::InvalidText {
                what: RowKind::Capability,
                row: clip(name),
                column: "name",
                problem: "is not lowercase ASCII letters, digits and '_' in dot-separated parts \
                          of at most 64 bytes"
                    .to_owned(),
            });
        }
        let groups = check_text(
            RowKind::Capability,
            || name.to_owned(),
            "groups",
            groups,
            false,
        )?;
        Ok(Self {
            name: name.into(),
            groups,
        })
    }

    /// The capability name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The group spec text.
    pub fn groups(&self) -> &str {
        &self.groups
    }
}

/// The groups of one custom field (`ir_model_fields` with `ir_model_fields_group_rel`,
/// ), the database overlay of field groups that
/// merges with the registry's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldGroupsRow {
    model: Box<str>,
    field: Box<str>,
    groups: Vec<GroupId>,
}

impl FieldGroupsRow {
    /// The groups of `model`.`field` (set semantics: sorted and deduplicated).
    ///
    /// # Errors
    ///
    /// [`RowError::InvalidText`] for an empty, too long or malformed model or field name.
    pub fn new(
        model: &str,
        field: &str,
        groups: impl IntoIterator<Item = GroupId>,
    ) -> Result<Self, RowError> {
        let label = || format!("{model}.{field}");
        let model = check_identifier(RowKind::FieldGroups, label, "model", model)?;
        let field = check_identifier(RowKind::FieldGroups, label, "field", field)?;
        let mut groups: Vec<GroupId> = groups.into_iter().collect();
        groups.sort_unstable();
        groups.dedup();
        Ok(Self {
            model,
            field,
            groups,
        })
    }

    /// The model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The field name.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// The groups, sorted.
    pub fn groups(&self) -> &[GroupId] {
        &self.groups
    }
}

/// Every policy row of one database, read in one snapshot, with the `policy` generation read in
/// the same snapshot. Build it with [`PolicyRows::builder`].
///
/// Rows are kept sorted (rules and groups by id, implications by pair, privileges by id,
/// capabilities by name, field groups by model and field), so two loads of the same tables give
/// equal values whatever order the statements returned them in.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyRows {
    generation: u64,
    rules: Vec<RuleRow>,
    groups: Vec<GroupRow>,
    implied: Vec<Implication>,
    privileges: Vec<PrivilegeRow>,
    capabilities: Vec<CapabilityRow>,
    field_groups: Vec<FieldGroupsRow>,
    archived_generated: Vec<GeneratedRow>,
}

impl PolicyRows {
    /// Start a row set read at the `policy` generation `generation` (the sum of the `policy`
    /// weights of `erp_signal_generation` in the loader's snapshot; 0 when the key has no row).
    pub fn builder(generation: u64) -> PolicyRowsBuilder {
        PolicyRowsBuilder {
            rows: PolicyRows {
                generation,
                rules: Vec::new(),
                groups: Vec::new(),
                implied: Vec::new(),
                privileges: Vec::new(),
                capabilities: Vec::new(),
                field_groups: Vec::new(),
                archived_generated: Vec::new(),
            },
        }
    }

    /// The `policy` generation the rows were read at.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The active `ir.access` rows, by id.
    pub fn rules(&self) -> &[RuleRow] {
        &self.rules
    }

    /// The groups, by id.
    pub fn groups(&self) -> &[GroupRow] {
        &self.groups
    }

    /// The implications, sorted.
    pub fn implied(&self) -> &[Implication] {
        &self.implied
    }

    /// The privileges, by id.
    pub fn privileges(&self) -> &[PrivilegeRow] {
        &self.privileges
    }

    /// The capability mappings, by name.
    pub fn capabilities(&self) -> &[CapabilityRow] {
        &self.capabilities
    }

    /// The field-groups overlay, by model and field.
    pub fn field_groups(&self) -> &[FieldGroupsRow] {
        &self.field_groups
    }

    /// The archived generated company restrictions, by id. Archived rows are invisible to the
    /// compiler; they are kept so the verifier can name an archived restriction (design §6.2).
    pub fn archived_generated(&self) -> &[GeneratedRow] {
        &self.archived_generated
    }

    /// Every generated company restriction, active (from [`Self::rules`]) and archived, by id:
    /// the input of [`crate::company_access::verify`].
    pub fn generated_rows(&self) -> Vec<GeneratedRow> {
        let mut rows: Vec<GeneratedRow> = self
            .rules
            .iter()
            .filter(|r| r.generated())
            .map(GeneratedRow::from_active)
            .chain(self.archived_generated.iter().cloned())
            .collect();
        rows.sort_by_key(GeneratedRow::id);
        rows
    }
}

/// Collects the rows of a [`PolicyRows`]; [`PolicyRowsBuilder::build`] validates them.
#[derive(Debug, Clone)]
#[must_use]
pub struct PolicyRowsBuilder {
    rows: PolicyRows,
}

impl PolicyRowsBuilder {
    /// Add an `ir.access` row.
    pub fn rule(mut self, rule: RuleRow) -> Self {
        self.rows.rules.push(rule);
        self
    }

    /// Add `ir.access` rows.
    pub fn rules(mut self, rules: impl IntoIterator<Item = RuleRow>) -> Self {
        self.rows.rules.extend(rules);
        self
    }

    /// Add an archived generated company restriction.
    pub fn archived_generated(mut self, row: GeneratedRow) -> Self {
        self.rows.archived_generated.push(row);
        self
    }

    /// Add a group.
    pub fn group(mut self, group: GroupRow) -> Self {
        self.rows.groups.push(group);
        self
    }

    /// Add groups.
    pub fn groups(mut self, groups: impl IntoIterator<Item = GroupRow>) -> Self {
        self.rows.groups.extend(groups);
        self
    }

    /// Add an implication.
    pub fn implication(mut self, implication: Implication) -> Self {
        self.rows.implied.push(implication);
        self
    }

    /// Add implications.
    pub fn implications(mut self, implied: impl IntoIterator<Item = Implication>) -> Self {
        self.rows.implied.extend(implied);
        self
    }

    /// Add a privilege.
    pub fn privilege(mut self, privilege: PrivilegeRow) -> Self {
        self.rows.privileges.push(privilege);
        self
    }

    /// Add privileges.
    pub fn privileges(mut self, privileges: impl IntoIterator<Item = PrivilegeRow>) -> Self {
        self.rows.privileges.extend(privileges);
        self
    }

    /// Add a capability mapping.
    pub fn capability(mut self, capability: CapabilityRow) -> Self {
        self.rows.capabilities.push(capability);
        self
    }

    /// Add capability mappings.
    pub fn capabilities(mut self, capabilities: impl IntoIterator<Item = CapabilityRow>) -> Self {
        self.rows.capabilities.extend(capabilities);
        self
    }

    /// Add a field-groups overlay row.
    pub fn field_group(mut self, row: FieldGroupsRow) -> Self {
        self.rows.field_groups.push(row);
        self
    }

    /// Add field-groups overlay rows.
    pub fn field_groups(mut self, rows: impl IntoIterator<Item = FieldGroupsRow>) -> Self {
        self.rows.field_groups.extend(rows);
        self
    }

    /// Validate and sort the rows.
    ///
    /// # Errors
    ///
    /// - [`RowError::TooMany`] / [`RowError::TooMuchText`] past the limits of this module;
    /// - [`RowError::Duplicate`] for a repeated rule, group or privilege id, capability name,
    ///   implication, or field (model and field name);
    /// - [`RowError::DuplicateXmlId`] when one XML id names two groups or two rules (or a group
    ///   lists it twice);
    /// - [`RowError::UnknownGroup`] for a rule, implication or field naming a group that is not
    ///   in the rows, [`RowError::UnknownPrivilege`] for a group naming a missing privilege.
    pub fn build(self) -> Result<PolicyRows, RowError> {
        let mut rows = self.rows;
        check_count(RowKind::Rule, rows.rules.len(), MAX_RULES)?;
        check_count(RowKind::Group, rows.groups.len(), MAX_GROUPS)?;
        check_count(RowKind::Implication, rows.implied.len(), MAX_IMPLICATIONS)?;
        check_count(RowKind::Privilege, rows.privileges.len(), MAX_PRIVILEGES)?;
        check_count(
            RowKind::Capability,
            rows.capabilities.len(),
            MAX_CAPABILITIES,
        )?;
        check_count(
            RowKind::FieldGroups,
            rows.field_groups.len(),
            MAX_FIELD_GROUPS,
        )?;
        check_total_text(&rows)?;

        rows.rules.sort_by_key(RuleRow::id);
        rows.archived_generated.sort_by_key(GeneratedRow::id);
        rows.groups.sort_by_key(GroupRow::id);
        rows.implied.sort_unstable();
        rows.privileges.sort_by_key(PrivilegeRow::id);
        rows.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        rows.field_groups
            .sort_by(|a, b| (&a.model, &a.field).cmp(&(&b.model, &b.field)));
        for group in &mut rows.groups {
            group.xmlids.sort();
        }

        unique_sorted(RowKind::Rule, &rows.rules, |r| r.id)?;
        unique_sorted(RowKind::Group, &rows.groups, |g| g.id)?;
        unique_sorted(RowKind::Privilege, &rows.privileges, |p| p.id)?;
        unique_sorted(RowKind::Capability, &rows.capabilities, |c| c.name.clone())?;
        unique_sorted(RowKind::Implication, &rows.implied, |i| {
            format!("({}, {})", i.group, i.implies)
        })?;
        unique_sorted(RowKind::FieldGroups, &rows.field_groups, |f| {
            format!("{}.{}", f.model, f.field)
        })?;
        unique_xmlids(&rows)?;

        let groups: BTreeSet<GroupId> = rows.groups.iter().map(GroupRow::id).collect();
        let privileges: BTreeSet<PrivilegeId> =
            rows.privileges.iter().map(PrivilegeRow::id).collect();
        let known = |referrer: RowKind, row: &dyn Fn() -> String, group: GroupId| {
            if groups.contains(&group) {
                Ok(())
            } else {
                Err(RowError::UnknownGroup {
                    referrer,
                    row: clip(&row()),
                    group,
                })
            }
        };
        for rule in &rows.rules {
            if let Some(group) = rule.group {
                known(RowKind::Rule, &|| rule.id.to_string(), group)?;
            }
        }
        for implication in &rows.implied {
            let row = || format!("({}, {})", implication.group, implication.implies);
            known(RowKind::Implication, &row, implication.group)?;
            known(RowKind::Implication, &row, implication.implies)?;
        }
        for field in &rows.field_groups {
            for group in &field.groups {
                known(
                    RowKind::FieldGroups,
                    &|| format!("{}.{}", field.model, field.field),
                    *group,
                )?;
            }
        }
        for group in &rows.groups {
            if let Some(privilege) = group.privilege
                && !privileges.contains(&privilege)
            {
                return Err(RowError::UnknownPrivilege {
                    group: group.id,
                    privilege,
                });
            }
        }
        Ok(rows)
    }
}

fn check_count(what: RowKind, count: usize, max: usize) -> Result<(), RowError> {
    if count > max {
        return Err(RowError::TooMany { what, count, max });
    }
    Ok(())
}

fn check_total_text(rows: &PolicyRows) -> Result<(), RowError> {
    let sizes = rows
        .rules
        .iter()
        .map(RuleRow::text_bytes)
        .chain(rows.groups.iter().map(|g| g.name.len()))
        .chain(rows.privileges.iter().map(|p| p.name.len()))
        .chain(
            rows.capabilities
                .iter()
                .map(|c| c.name.len() + c.groups.len()),
        )
        .chain(
            rows.field_groups
                .iter()
                .map(|f| f.model.len() + f.field.len()),
        );
    let mut total: usize = 0;
    for size in sizes {
        total = total.saturating_add(size);
        if total > MAX_TOTAL_TEXT_BYTES {
            return Err(RowError::TooMuchText {
                bytes: total,
                max: MAX_TOTAL_TEXT_BYTES,
            });
        }
    }
    Ok(())
}

/// `items` sorted by `key`: the first repeated key is an error.
fn unique_sorted<T, K: PartialEq + fmt::Display>(
    what: RowKind,
    items: &[T],
    key: impl Fn(&T) -> K,
) -> Result<(), RowError> {
    let mut previous: Option<K> = None;
    for item in items {
        let k = key(item);
        if previous.as_ref() == Some(&k) {
            return Err(RowError::Duplicate {
                what,
                key: clip(&k.to_string()),
            });
        }
        previous = Some(k);
    }
    Ok(())
}

fn unique_xmlids(rows: &PolicyRows) -> Result<(), RowError> {
    let mut seen: BTreeMap<&QualifiedXmlId, ()> = BTreeMap::new();
    let all = rows
        .groups
        .iter()
        .flat_map(|g| g.xmlids.iter())
        .chain(rows.rules.iter().filter_map(|r| r.xmlid.as_ref()));
    for xmlid in all {
        if seen.insert(xmlid, ()).is_some() {
            return Err(RowError::DuplicateXmlId {
                xmlid: xmlid.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gid(n: i64) -> GroupId {
        GroupId::new(n).unwrap()
    }

    fn rid(n: i64) -> RuleId {
        RuleId::new(n).unwrap()
    }

    fn xid(s: &str) -> QualifiedXmlId {
        QualifiedXmlId::parse(s).unwrap()
    }

    #[test]
    fn the_fifteen_operations_parse_and_render_back() {
        let all = [
            "crud", "cru", "crd", "cud", "rud", "cr", "cu", "cd", "ru", "rd", "ud", "c", "r", "u",
            "d",
        ];
        for op in all {
            let rule = RuleRow::new(rid(1), "n", "m.m", op).unwrap();
            assert_eq!(rule.operation(), op);
        }
        let rule = RuleRow::new(rid(1), "n", "m.m", "ru").unwrap();
        assert!(rule.ops().contains(Operation::Read));
        assert!(rule.ops().contains(Operation::Write));
        assert!(!rule.ops().contains(Operation::Create));
        assert!(!rule.ops().contains(Operation::Unlink));
    }

    /// `for_*` fields of the `ir.access` model and their inverse come with the access screen,
    /// which is where `addons/base` defers them)
    #[test]
    fn spec_sec_006_operation_booleans() {
        let rule = RuleRow::new(rid(1), "n", "m.m", "ru").unwrap();
        let flags = |ops: OperationSet| {
            [
                Operation::Read,
                Operation::Write,
                Operation::Create,
                Operation::Unlink,
            ]
            .map(|op| covers(ops, op))
        };
        assert_eq!(flags(rule.ops()), [true, true, false, false]);
        let rule = RuleRow::new(rid(1), "n", "m.m", "cr").unwrap();
        assert_eq!(flags(rule.ops()), [true, false, true, false]);
        // for_write = True, for_create = False
        let edited = OperationSet::of([Operation::Read, Operation::Write]);
        assert_eq!(operation_string(operation_from_flags(edited)), "ru");
        assert_eq!(
            operation_string(operation_from_flags(OperationSet::EMPTY)),
            "r"
        );
        // a create with only for_create
        let created = operation_at_create(OperationSet::of([Operation::Create]));
        assert_eq!(created.map(operation_string).as_deref(), Some("c"));
        assert_eq!(operation_at_create(OperationSet::EMPTY), None);
    }

    /// on the `ir.access` model, which copies and archives, comes with the access screen,
    /// which is where `addons/base` defers it)
    #[test]
    fn spec_sec_043_standard_rules() {
        let row = |xmlid: Option<&str>| {
            let mut row = RuleRow::new(rid(1), "n", "m.m", "r").unwrap();
            if let Some(x) = xmlid {
                row = row.with_xmlid(xid(x));
            }
            row
        };
        assert!(row(Some("sale.sale_order_personal_rule")).is_standard());
        assert!(!row(Some("__export__.ir_access_5_abcd")).is_standard());
        assert!(!row(Some("__custom__.rule")).is_standard());
        assert!(!row(Some("studio_customization.rule_x")).is_standard());
        assert!(!row(None).is_standard());
    }

    #[test]
    fn other_operation_strings_are_refused() {
        let long = "c".repeat(100);
        for bad in [
            "", "rc", "rr", "x", "CRUD", "cr ", "dcur", "crudd", "r\u{0}", &long,
        ] {
            let err = RuleRow::new(rid(4), "n", "m.m", bad).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Programming);
            assert_eq!(
                err.to_string(),
                format!(
                    "policy rows: ir.access 4 has the invalid operation '{}': expected a \
                     non-empty subset of 'crud' written in c, r, u, d order",
                    clip(bad)
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn ids_are_positive() {
        for bad in [0, -1, i64::MIN] {
            let err = RuleId::new(bad).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("policy rows: invalid ir.access id {bad}: ids are positive")
            );
            let err = PrivilegeId::new(bad).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("policy rows: invalid res.groups.privilege id {bad}: ids are positive")
            );
            let err = group_id(bad).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("policy rows: invalid res.groups id {bad}: ids are positive")
            );
            assert_eq!(err.kind(), ErrorKind::Programming);
        }
        assert_eq!(RuleId::new(i64::MAX).unwrap().get(), i64::MAX);
    }

    #[test]
    fn model_names_and_text_are_checked() {
        let err = RuleRow::new(rid(2), "n", "", "r").unwrap_err();
        assert_eq!(err.to_string(), "policy rows: ir.access 2: model is empty");
        let err = RuleRow::new(rid(2), "n", "res partner", "r").unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: ir.access 2: model 'res partner' has whitespace or control characters"
        );
        let long = "é".repeat(MAX_TEXT_BYTES / 2 + 1);
        let err = GroupRow::new(gid(3), &long).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "policy rows: res.groups 3: name is {} bytes, more than the 4096 allowed",
                long.len()
            )
        );
        // non-ASCII names are ordinary display text
        let group = GroupRow::new(gid(3), "Gérant / 経理").unwrap();
        assert_eq!(group.name(), "Gérant / 経理");
        let err = FieldGroupsRow::new("x.y", "", [gid(1)]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: ir_model_fields_group_rel x.y.: field is empty"
        );
    }

    #[test]
    fn api_key_durations_are_finite_and_not_negative() {
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            let err = GroupRow::new(gid(5), "g")
                .unwrap()
                .with_api_key_duration(bad)
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "policy rows: res.groups 5 has the API key duration {bad}: it must be a \
                     finite number of days, at least 0"
                )
            );
        }
        let group = GroupRow::new(gid(5), "g")
            .unwrap()
            .with_api_key_duration(0.0)
            .unwrap();
        assert_eq!(group.api_key_duration(), Some(0.0));
        assert_eq!(GroupRow::new(gid(5), "g").unwrap().api_key_duration(), None);
    }

    #[test]
    fn capability_names_follow_the_capability_rules() {
        let row = CapabilityRow::new("policy.explain_others", "base.group_system").unwrap();
        assert_eq!(
            (row.name(), row.groups()),
            ("policy.explain_others", "base.group_system")
        );
        for bad in ["", "Policy.x", "a..b", "a b", "a-b"] {
            let err = CapabilityRow::new(bad, "base.group_system").unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "policy rows: ir_capability {bad}: name is not lowercase ASCII letters, \
                     digits and '_' in dot-separated parts of at most 64 bytes"
                )
            );
        }
        let err = CapabilityRow::new("a.b", "").unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: ir_capability a.b: groups is empty"
        );
    }

    #[test]
    fn rows_are_sorted_and_equal_whatever_the_input_order() {
        let groups = [
            GroupRow::new(gid(2), "b").unwrap(),
            GroupRow::new(gid(1), "a").unwrap(),
        ];
        let rules = [
            RuleRow::new(rid(9), "r9", "m.m", "r").unwrap(),
            RuleRow::new(rid(3), "r3", "m.m", "r").unwrap(),
        ];
        let a = PolicyRows::builder(1)
            .groups(groups.clone())
            .rules(rules.clone())
            .implications([
                Implication::new(gid(2), gid(1)),
                Implication::new(gid(1), gid(2)),
            ])
            .build()
            .unwrap();
        let b = PolicyRows::builder(1)
            .groups(groups.into_iter().rev())
            .rules(rules.into_iter().rev())
            .implications([
                Implication::new(gid(1), gid(2)),
                Implication::new(gid(2), gid(1)),
            ])
            .build()
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(
            a.rules().iter().map(|r| r.id().get()).collect::<Vec<_>>(),
            [3, 9]
        );
        assert_eq!(
            a.groups().iter().map(|g| g.id().get()).collect::<Vec<_>>(),
            [1, 2]
        );
        let empty = PolicyRows::builder(0).build().unwrap();
        assert!(empty.rules().is_empty() && empty.groups().is_empty());
    }

    #[test]
    fn duplicates_are_refused() {
        let g = || GroupRow::new(gid(1), "a").unwrap();
        let err = PolicyRows::builder(0)
            .group(g())
            .group(g())
            .build()
            .unwrap_err();
        assert_eq!(err.to_string(), "policy rows: res.groups 1 is listed twice");
        let r = || RuleRow::new(rid(4), "a", "m.m", "r").unwrap();
        let err = PolicyRows::builder(0)
            .rule(r())
            .rule(r())
            .build()
            .unwrap_err();
        assert_eq!(err.to_string(), "policy rows: ir.access 4 is listed twice");
        let i = Implication::new(gid(1), gid(1));
        let err = PolicyRows::builder(0)
            .group(g())
            .implications([i, i])
            .build()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: res_groups_implied_rel (1, 1) is listed twice"
        );
        let c = || CapabilityRow::new("a.b", "base.group_system").unwrap();
        let err = PolicyRows::builder(0)
            .capability(c())
            .capability(c())
            .build()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: ir_capability a.b is listed twice"
        );
        let f = || FieldGroupsRow::new("m.m", "x_f", [gid(1)]).unwrap();
        let err = PolicyRows::builder(0)
            .group(g())
            .field_groups([f(), f()])
            .build()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: ir_model_fields_group_rel m.m.x_f is listed twice"
        );
        let p = || PrivilegeRow::new(PrivilegeId::new(8).unwrap(), "p").unwrap();
        let err = PolicyRows::builder(0)
            .privileges([p(), p()])
            .build()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: res.groups.privilege 8 is listed twice"
        );
        assert_eq!(err.kind(), ErrorKind::Programming);
    }

    #[test]
    fn one_xml_id_names_one_row() {
        let x = xid("base.group_user");
        let err = PolicyRows::builder(0)
            .group(GroupRow::new(gid(1), "a").unwrap().with_xmlid(x.clone()))
            .group(GroupRow::new(gid(2), "b").unwrap().with_xmlid(x.clone()))
            .build()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: the XML id base.group_user names two rows"
        );
        let err = PolicyRows::builder(0)
            .group(GroupRow::new(gid(1), "a").unwrap().with_xmlid(x.clone()))
            .rule(
                RuleRow::new(rid(1), "r", "m.m", "r")
                    .unwrap()
                    .with_xmlid(x.clone()),
            )
            .build()
            .unwrap_err();
        assert_eq!(err, RowError::DuplicateXmlId { xmlid: x.clone() });
        let err = PolicyRows::builder(0)
            .group(
                GroupRow::new(gid(1), "a")
                    .unwrap()
                    .with_xmlid(x.clone())
                    .with_xmlid(x.clone()),
            )
            .build()
            .unwrap_err();
        assert_eq!(err, RowError::DuplicateXmlId { xmlid: x });
        let ok = PolicyRows::builder(0)
            .group(
                GroupRow::new(gid(1), "a")
                    .unwrap()
                    .with_xmlid(xid("z.b"))
                    .with_xmlid(xid("a.b")),
            )
            .build()
            .unwrap();
        assert_eq!(ok.groups()[0].xmlids(), [xid("a.b"), xid("z.b")]);
    }

    #[test]
    fn references_to_missing_groups_and_privileges_are_refused() {
        let err = PolicyRows::builder(0)
            .rule(
                RuleRow::new(rid(5), "r", "m.m", "r")
                    .unwrap()
                    .with_group(gid(9)),
            )
            .build()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: ir.access 5 refers to the group 9, which is not in the rows"
        );
        assert_eq!(err.kind(), ErrorKind::Programming);
        let err = PolicyRows::builder(0)
            .group(GroupRow::new(gid(1), "a").unwrap())
            .implication(Implication::new(gid(1), gid(2)))
            .build()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: res_groups_implied_rel (1, 2) refers to the group 2, which is not in \
             the rows"
        );
        let err = PolicyRows::builder(0)
            .field_group(FieldGroupsRow::new("m.m", "x_a", [gid(3)]).unwrap())
            .build()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: ir_model_fields_group_rel m.m.x_a refers to the group 3, which is not \
             in the rows"
        );
        let err = PolicyRows::builder(0)
            .group(
                GroupRow::new(gid(1), "a")
                    .unwrap()
                    .with_privilege(PrivilegeId::new(4).unwrap()),
            )
            .build()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "policy rows: res.groups 1 refers to the privilege 4, which is not in the rows"
        );
    }

    #[test]
    fn limits_are_errors_naming_the_limit() {
        let rules: Vec<RuleRow> = (1..=i64::try_from(MAX_RULES).unwrap() + 1)
            .map(|n| RuleRow::new(rid(n), "", "m", "r").unwrap())
            .collect();
        let err = PolicyRows::builder(0).rules(rules).build().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
        assert_eq!(
            err.to_string(),
            "policy rows: 100001 ir.access rows are more than the 100000 a policy may hold"
        );
        let domain = "x".repeat(1024 * 1024);
        let rules: Vec<RuleRow> = (1..=65)
            .map(|n| {
                RuleRow::new(rid(n), "", "m", "r")
                    .unwrap()
                    .with_domain(&domain)
            })
            .collect();
        let err = PolicyRows::builder(0).rules(rules).build().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
        assert_eq!(
            err.to_string(),
            format!(
                "policy rows: more than {MAX_TOTAL_TEXT_BYTES} bytes of names, domains and group \
                 specs (at least {})",
                64 * (1024 * 1024 + 1)
            )
        );
        let as_error = Error::from(err.clone());
        assert_eq!(as_error.kind(), ErrorKind::ResourceExhausted);
        assert_eq!(as_error.message(), err.to_string());
        assert!(std::error::Error::source(&as_error).is_some());
    }
}
