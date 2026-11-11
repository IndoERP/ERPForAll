//! Company access declarations (amendment A1, decisions A1-D2 and A1-D3; design note
//! , §5).
//!
//! A company-scoped model declares once how the reader's active companies restrict it:
//!
//! - through its own company field: `direct(field = …, branch = "exact" | "ancestor", empty =
//!   "allow" | "deny")`;
//! - through the record that owns it: `parent(edge = …)`, a stored, required many2one;
//! - both: `direct(…, parent = …)`;
//! - or, explicitly and with a reason, not at all: `unrestricted(reason = …)`.
//!
//! This module holds the declared types ([`CompanyAccessDecl`], const-constructible for
//! `#[model]`/`#[extend]`, and its owned twin [`CompanyAccessDef`] for model files), the resolved
//! types the registry stores on [`crate::ModelSpec::company_access`] ([`CompanyAccess`]), and the
//! pure validation pass the composer runs after the company scopes are known. Declarations compose
//! through the MRO oldest first, and each newer one must be at most as wide as the one before it
//! (an extension may only narrow). Nothing here writes rows or enforces anything: the generator,
//! the installation and the enforcement are later sub-slices.

use std::fmt;

use erp_fields::{Error, ErrorKind, FieldType, Result};
use rustc_hash::FxHashMap;

use crate::decl::{ModelDecl, describe_definition};
use crate::spec::{CompanyScope, FieldId, ModelId, ModelSpec, Registry};

/// The model every company field links to.
const COMPANY_MODEL: &str = "res.company";

/// Most owner edges a chain of `parent` declarations may follow before it reaches a model with a
/// direct restriction (design §5).
pub const MAX_OWNER_DEPTH: usize = 8;

/// Longest reason an `unrestricted(reason = …)` declaration may give, in bytes (Q1). A reason
/// must also not be blank: empty or only whitespace (`str::trim`, Unicode `White_Space`).
pub const MAX_UNRESTRICTED_REASON_BYTES: usize = 512;

/// The length in bytes of an `unrestricted` reason the registry refuses: longer than
/// [`MAX_UNRESTRICTED_REASON_BYTES`], or blank (review M1). `None` when it is accepted.
fn refused_reason(reason: &str) -> Option<usize> {
    let bytes = reason.len();
    (bytes > MAX_UNRESTRICTED_REASON_BYTES || reason.trim().is_empty()).then_some(bytes)
}

/// The words of every refused reason, given its length from [`refused_reason`]: a length over
/// the limit is reported as such, any other refused reason is blank. The macro's message is the
/// part before the comma.
struct ReasonRefusal(usize);

impl fmt::Display for ReasonRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unrestricted(…) needs a reason that is not blank and at most \
             {MAX_UNRESTRICTED_REASON_BYTES} bytes long, but it is "
        )?;
        if self.0 > MAX_UNRESTRICTED_REASON_BYTES {
            write!(f, "{} bytes long", self.0)
        } else {
            f.write_str("blank")
        }
    }
}

// ------------------------------------------------------------------------------------------------
// declared types

/// Whether the ancestors of the active companies count (A1-D2 "branch mode").
///
/// ```
/// use erp_registry::CompanyBranch;
/// assert_eq!(CompanyBranch::parse("ancestor").unwrap(), CompanyBranch::Ancestor);
/// assert_eq!(CompanyBranch::Exact.as_str(), "exact");
/// assert!(CompanyBranch::parse("parent_of").is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CompanyBranch {
    /// A record of one of the active companies.
    Exact,
    /// A record of an active company or of one of their ancestors (needs `res.company` to keep
    /// its hierarchy).
    Ancestor,
}

impl CompanyBranch {
    /// The two spellings, in declaration order.
    pub const NAMES: [&'static str; 2] = ["exact", "ancestor"];

    /// Parse `"exact"` or `"ancestor"`.
    ///
    /// # Errors
    ///
    /// `Programming` naming the two spellings for anything else.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "exact" => Ok(Self::Exact),
            "ancestor" => Ok(Self::Ancestor),
            other => Err(Error::programming(format!(
                "branch must be \"exact\" or \"ancestor\" (got \"{other}\")"
            ))),
        }
    }

    /// The spelling [`CompanyBranch::parse`] accepts.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Ancestor => "ancestor",
        }
    }

    /// `self` admits at most what `older` admits (`exact` ⊑ `ancestor`).
    const fn within(self, older: Self) -> bool {
        matches!(
            (self, older),
            (Self::Exact, _) | (Self::Ancestor, Self::Ancestor)
        )
    }
}

/// Whether a record without company (NULL `company_id`, empty `company_ids`) is admitted. Never
/// inferred: every direct declaration states it (A1-D2).
///
/// ```
/// use erp_registry::EmptyCompany;
/// assert_eq!(EmptyCompany::parse("allow").unwrap(), EmptyCompany::Allow);
/// assert_eq!(EmptyCompany::Deny.as_str(), "deny");
/// assert!(EmptyCompany::parse("False").is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EmptyCompany {
    /// A record without company is refused.
    Deny,
    /// A record without company is admitted (while the active company set is not empty).
    Allow,
}

impl EmptyCompany {
    /// The two spellings, in declaration order.
    pub const NAMES: [&'static str; 2] = ["deny", "allow"];

    /// Parse `"deny"` or `"allow"`.
    ///
    /// # Errors
    ///
    /// `Programming` naming the two spellings for anything else.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "deny" => Ok(Self::Deny),
            "allow" => Ok(Self::Allow),
            other => Err(Error::programming(format!(
                "empty must be \"allow\" or \"deny\" (got \"{other}\")"
            ))),
        }
    }

    /// The spelling [`EmptyCompany::parse`] accepts.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Allow => "allow",
        }
    }

    /// `self` admits at most what `older` admits (`deny` ⊑ `allow`).
    const fn within(self, older: Self) -> bool {
        matches!((self, older), (Self::Deny, _) | (Self::Allow, Self::Allow))
    }
}

/// `direct(field = …, branch = …, empty = …)` as one definition wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirectDecl {
    field: &'static str,
    branch: CompanyBranch,
    empty: EmptyCompany,
    escape: Option<&'static str>,
}

impl DirectDecl {
    /// The company field named (`company_id` or `company_ids`).
    pub const fn field(&self) -> &'static str {
        self.field
    }

    /// The branch mode.
    pub const fn branch(&self) -> CompanyBranch {
        self.branch
    }

    /// The empty-company policy.
    pub const fn empty(&self) -> EmptyCompany {
        self.empty
    }

    /// The escape field named, if any: a record whose stored boolean is `false` is admitted
    /// whatever its companies.
    pub const fn escape(&self) -> Option<&'static str> {
        self.escape
    }

    /// `self` admits at most what `older` admits: the same field, a branch and an empty policy
    /// at most as wide, and no escape that `older` did not have (an escape admits more).
    fn within(&self, older: &Self) -> bool {
        self.field == older.field
            && self.branch.within(older.branch)
            && self.empty.within(older.empty)
            && match (self.escape, older.escape) {
                (None, _) => true,
                (Some(new), Some(old)) => new == old,
                (Some(_), None) => false,
            }
    }

    fn write_args(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "field = {:?}, branch = \"{}\", empty = \"{}\"",
            self.field,
            self.branch.as_str(),
            self.empty.as_str()
        )?;
        match self.escape {
            Some(escape) => write!(f, ", escape = {escape:?}"),
            None => Ok(()),
        }
    }
}

/// `parent(edge = …)`: the required many2one to the record that owns this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParentDecl {
    edge: &'static str,
}

impl ParentDecl {
    /// The owner edge named.
    pub const fn edge(&self) -> &'static str {
        self.edge
    }
}

/// `unrestricted(reason = …)`: no generated restriction, said explicitly (design Q1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnrestrictedDecl {
    reason: &'static str,
}

impl UnrestrictedDecl {
    /// Why the model is not restricted by company (not blank, at most
    /// [`MAX_UNRESTRICTED_REASON_BYTES`] bytes; the registry checks it).
    pub const fn reason(&self) -> &'static str {
        self.reason
    }
}

/// The `company_access` of one model definition (A1-D2). Build it with the constructors;
/// `#[model(company_access = …)]` emits them. The registry validates it when it is built
/// ([`CompanyAccessError`]).
///
/// ```
/// use erp_registry::{CompanyAccessDecl, CompanyBranch, EmptyCompany, ModelDecl};
///
/// static DOC: ModelDecl = ModelDecl::new("shop", "shop.doc").company_access(
///     CompanyAccessDecl::direct("company_id", CompanyBranch::Exact, EmptyCompany::Deny),
/// );
/// assert_eq!(
///     DOC.company_access.unwrap().spelling(),
///     r#"direct(field = "company_id", branch = "exact", empty = "deny")"#
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CompanyAccessDecl {
    /// Restricted through the model's own company field.
    Direct(DirectDecl),
    /// Restricted through the record that owns it.
    Parent(ParentDecl),
    /// Both: the model's own company field and its owner (AND).
    DirectAndParent(DirectDecl, ParentDecl),
    /// Not restricted by company, with the reason.
    Unrestricted(UnrestrictedDecl),
}

impl CompanyAccessDecl {
    /// `direct(field = …, branch = …, empty = …)`.
    pub const fn direct(field: &'static str, branch: CompanyBranch, empty: EmptyCompany) -> Self {
        Self::Direct(DirectDecl {
            field,
            branch,
            empty,
            escape: None,
        })
    }

    pub const fn direct_escaping(
        field: &'static str,
        branch: CompanyBranch,
        empty: EmptyCompany,
        escape: &'static str,
    ) -> Self {
        Self::Direct(DirectDecl {
            field,
            branch,
            empty,
            escape: Some(escape),
        })
    }

    /// `parent(edge = …)`.
    pub const fn parent(edge: &'static str) -> Self {
        Self::Parent(ParentDecl { edge })
    }

    /// `direct(field = …, branch = …, empty = …, parent = …)`.
    pub const fn direct_and_parent(
        field: &'static str,
        branch: CompanyBranch,
        empty: EmptyCompany,
        edge: &'static str,
    ) -> Self {
        Self::DirectAndParent(
            DirectDecl {
                field,
                branch,
                empty,
                escape: None,
            },
            ParentDecl { edge },
        )
    }

    /// `unrestricted(reason = …)`.
    pub const fn unrestricted(reason: &'static str) -> Self {
        Self::Unrestricted(UnrestrictedDecl { reason })
    }

    /// The direct half, if any.
    pub const fn direct_part(&self) -> Option<DirectDecl> {
        match self {
            Self::Direct(d) | Self::DirectAndParent(d, _) => Some(*d),
            Self::Parent(_) | Self::Unrestricted(_) => None,
        }
    }

    /// The owner edge, if any.
    pub const fn owner_edge(&self) -> Option<&'static str> {
        match self {
            Self::Parent(p) | Self::DirectAndParent(_, p) => Some(p.edge),
            Self::Direct(_) | Self::Unrestricted(_) => None,
        }
    }

    /// The macro spelling, used in every message: `direct(field = "company_id", branch =
    /// "exact", empty = "deny")`, `parent(edge = "doc_id")`, `direct(…, parent = "tax_id")`,
    /// `unrestricted(reason = "…")`. Names and the reason are quoted and escaped like Rust
    /// string literals.
    pub fn spelling(&self) -> String {
        self.to_string()
    }

    /// `self`, declared after `older`, is at most as wide (⊑, design §3.4).
    fn within(&self, older: &Self) -> bool {
        match (self, older) {
            (_, Self::Unrestricted(_)) => true,
            (Self::Direct(new), Self::Direct(old)) => new.within(old),
            (Self::Parent(new), Self::Parent(old)) => new.edge == old.edge,
            (Self::DirectAndParent(new, _), Self::Direct(old)) => new.within(old),
            (Self::DirectAndParent(_, new), Self::Parent(old)) => new.edge == old.edge,
            (Self::DirectAndParent(nd, np), Self::DirectAndParent(od, op)) => {
                nd.within(od) && np.edge == op.edge
            }
            // `direct` and `parent` are incomparable; dropping a half or opening a restricted
            // model widens it
            (Self::Unrestricted(_), _)
            | (Self::Direct(_), Self::Parent(_) | Self::DirectAndParent(..))
            | (Self::Parent(_), Self::Direct(_) | Self::DirectAndParent(..)) => false,
        }
    }
}

impl fmt::Display for CompanyAccessDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct(d) => {
                f.write_str("direct(")?;
                d.write_args(f)?;
                f.write_str(")")
            }
            Self::Parent(p) => write!(f, "parent(edge = {:?})", p.edge),
            Self::DirectAndParent(d, p) => {
                f.write_str("direct(")?;
                d.write_args(f)?;
                write!(f, ", parent = {:?})", p.edge)
            }
            Self::Unrestricted(u) => write!(f, "unrestricted(reason = {:?})", u.reason),
        }
    }
}

/// The owned `company_access` of a runtime definition ([`crate::ModelDef`], model files): the
/// same forms as [`CompanyAccessDecl`], interned into one when the definition is.
///
/// ```
/// use erp_registry::{CompanyAccessDef, CompanyBranch, EmptyCompany, ModelDef};
/// let def = ModelDef::new("my_rules", "my.doc").company_access(CompanyAccessDef::direct(
///     "company_id",
///     CompanyBranch::Ancestor,
///     EmptyCompany::Allow,
/// ));
/// assert!(def.check_limits().is_ok());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompanyAccessDef(DefForm);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DefForm {
    Direct {
        field: String,
        branch: CompanyBranch,
        empty: EmptyCompany,
        escape: Option<String>,
    },
    Parent {
        edge: String,
    },
    DirectAndParent {
        field: String,
        branch: CompanyBranch,
        empty: EmptyCompany,
        edge: String,
    },
    Unrestricted {
        reason: String,
    },
}

impl CompanyAccessDef {
    /// `direct(field = …, branch = …, empty = …)`.
    pub fn direct(field: impl Into<String>, branch: CompanyBranch, empty: EmptyCompany) -> Self {
        Self(DefForm::Direct {
            field: field.into(),
            branch,
            empty,
            escape: None,
        })
    }

    /// `direct(field = …, branch = …, empty = …, escape = …)`.
    pub fn direct_escaping(
        field: impl Into<String>,
        branch: CompanyBranch,
        empty: EmptyCompany,
        escape: impl Into<String>,
    ) -> Self {
        Self(DefForm::Direct {
            field: field.into(),
            branch,
            empty,
            escape: Some(escape.into()),
        })
    }

    /// `parent(edge = …)`.
    pub fn parent(edge: impl Into<String>) -> Self {
        Self(DefForm::Parent { edge: edge.into() })
    }

    /// `direct(field = …, branch = …, empty = …, parent = …)`.
    pub fn direct_and_parent(
        field: impl Into<String>,
        branch: CompanyBranch,
        empty: EmptyCompany,
        edge: impl Into<String>,
    ) -> Self {
        Self(DefForm::DirectAndParent {
            field: field.into(),
            branch,
            empty,
            edge: edge.into(),
        })
    }

    /// `unrestricted(reason = …)`.
    pub fn unrestricted(reason: impl Into<String>) -> Self {
        Self(DefForm::Unrestricted {
            reason: reason.into(),
        })
    }

    /// Checks of the declaration alone, before it is interned: names not empty and at most
    /// [`crate::MAX_NAME_BYTES`], the reason not blank and at most
    /// [`MAX_UNRESTRICTED_REASON_BYTES`] bytes (the macro's rules, so model files and overlays fail early with the same words). Whether the
    /// names make sense for the model is checked when the registry is built.
    ///
    /// # Errors
    ///
    /// `Validation`, naming the model.
    pub(crate) fn check_limits(&self, model: &str) -> Result<()> {
        let at = || format!("model '{}': company_access", crate::owned::clip(model));
        let name = |what: &str, text: &str| {
            if text.is_empty() {
                return Err(Error::validation(format!(
                    "{}: the {what} name is empty",
                    at()
                )));
            }
            crate::owned::check_len(text, crate::owned::MAX_NAME_BYTES, || {
                format!("{} {what} name", at())
            })
        };
        match &self.0 {
            DefForm::Direct { field, escape, .. } => {
                name("field", field)?;
                escape.as_deref().map_or(Ok(()), |e| name("escape", e))
            }
            DefForm::Parent { edge } => name("edge", edge),
            DefForm::DirectAndParent { field, edge, .. } => {
                name("field", field)?;
                name("edge", edge)
            }
            DefForm::Unrestricted { reason } => match refused_reason(reason) {
                Some(bytes) => Err(Error::validation(format!(
                    "{}: {}",
                    at(),
                    ReasonRefusal(bytes)
                ))),
                None => Ok(()),
            },
        }
    }

    /// The declaration, its strings taken from `intern`.
    pub(crate) fn intern(
        &self,
        intern: &mut impl FnMut(&str) -> Result<&'static str>,
    ) -> Result<CompanyAccessDecl> {
        Ok(match &self.0 {
            DefForm::Direct {
                field,
                branch,
                empty,
                escape: None,
            } => CompanyAccessDecl::direct(intern(field)?, *branch, *empty),
            DefForm::Direct {
                field,
                branch,
                empty,
                escape: Some(escape),
            } => {
                CompanyAccessDecl::direct_escaping(intern(field)?, *branch, *empty, intern(escape)?)
            }
            DefForm::Parent { edge } => CompanyAccessDecl::parent(intern(edge)?),
            DefForm::DirectAndParent {
                field,
                branch,
                empty,
                edge,
            } => {
                CompanyAccessDecl::direct_and_parent(intern(field)?, *branch, *empty, intern(edge)?)
            }
            DefForm::Unrestricted { reason } => CompanyAccessDecl::unrestricted(intern(reason)?),
        })
    }

    /// Heap bytes of the strings, each priced by `text`.
    pub(crate) fn heap_bytes(&self, text: impl Fn(&str) -> usize) -> usize {
        match &self.0 {
            DefForm::Direct { field, escape, .. } => {
                text(field).saturating_add(escape.as_deref().map_or(0, text))
            }
            DefForm::Parent { edge } => text(edge),
            DefForm::DirectAndParent { field, edge, .. } => text(field).saturating_add(text(edge)),
            DefForm::Unrestricted { reason } => text(reason),
        }
    }
}

// ------------------------------------------------------------------------------------------------
// resolved types

/// How many companies a record of the model has: one `company_id` or a `company_ids` set (from
/// the model's company scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CompanyCardinality {
    /// A `company_id` many2one.
    Single,
    /// A `company_ids` many2many.
    Many,
}

/// A validated direct restriction: the model's company field and the declared modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirectRestriction {
    field: FieldId,
    cardinality: CompanyCardinality,
    branch: CompanyBranch,
    empty: EmptyCompany,
    escape: Option<FieldId>,
}

impl DirectRestriction {
    /// The company field (the field of the model's company scope).
    pub const fn field(&self) -> FieldId {
        self.field
    }

    /// One company or a set.
    pub const fn cardinality(&self) -> CompanyCardinality {
        self.cardinality
    }

    /// The branch mode.
    pub const fn branch(&self) -> CompanyBranch {
        self.branch
    }

    /// The empty-company policy.
    pub const fn empty(&self) -> EmptyCompany {
        self.empty
    }

    /// The escape field: a stored, required boolean of the model whose `false` admits the
    /// record whatever its companies.
    pub const fn escape(&self) -> Option<FieldId> {
        self.escape
    }
}

/// A validated owner restriction: a stored, required many2one whose target's chain ends in a
/// direct restriction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OwnerRestriction {
    edge: FieldId,
    owner: ModelId,
}

impl OwnerRestriction {
    /// The owner edge of this model.
    pub const fn edge(&self) -> FieldId {
        self.edge
    }

    /// The owner model (the edge's comodel).
    pub const fn owner(&self) -> ModelId {
        self.owner
    }
}

/// What a model's effective `company_access` resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Restriction {
    /// Through the model's own company field.
    Direct(DirectRestriction),
    /// Through its owner.
    Owner(OwnerRestriction),
    /// Through both (AND).
    DirectAndOwner(DirectRestriction, OwnerRestriction),
    /// Declared unrestricted, with the reason (only the registry builds it).
    #[non_exhaustive]
    Unrestricted {
        /// The declared reason.
        reason: &'static str,
    },
}

/// A model definition that declares `company_access`: on a resolved [`CompanyAccess`] the
/// effective one (the newest in the MRO that declares one), on a [`CompanyAccessError`] the one
/// refused. Displays as [`DeclSource::describe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeclSource {
    addon: &'static str,
    definition: &'static str,
    file: &'static str,
    line: u32,
}

impl DeclSource {
    fn of(decl: &'static ModelDecl) -> Self {
        Self {
            addon: decl.addon,
            definition: decl.name,
            file: decl.file,
            line: decl.line,
        }
    }

    /// The declaring addon.
    pub const fn addon(&self) -> &'static str {
        self.addon
    }

    /// The model name of the declaring definition (an abstract mixin's own name when the
    /// declaration comes from one).
    pub const fn definition(&self) -> &'static str {
        self.definition
    }

    /// The source file (empty when the definition recorded none).
    pub const fn file(&self) -> &'static str {
        self.file
    }

    /// The source line.
    pub const fn line(&self) -> u32 {
        self.line
    }

    /// The definition as error messages name it (`model definition 'x.doc' of addon 'x'
    /// (src/doc.rs:12)`), the same text as [`ModelDecl::describe`].
    pub fn describe(&self) -> String {
        describe_definition(self.definition, self.addon, self.file, self.line)
    }
}

impl fmt::Display for DeclSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// What the registry knows about a model's company access (design §3.5, §5): the one value the
/// later slices (generation, enforcement, migration) read. There is no "absent means open":
/// [`Self::Undeclared`] is the internal state before validation for a concrete company-scoped
/// model whose addons declared nothing; a successful registry build refuses that state. Consumers
/// must never treat it as unrestricted. Only an explicit `unrestricted(reason = "…")` is open.
///
/// Not `#[non_exhaustive]` on purpose: a consumer matches it exhaustively, with no wildcard arm,
/// so adding a variant breaks every consumer instead of falling into a default (review R2 M1;
/// ).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompanyAccessState {
    /// A concrete model with a validated declaration.
    Declared(CompanyAccess),
    /// A model company access does not apply to, for the stated reason.
    NotApplicable(NotApplicable),
    /// A concrete company-scoped model (`company_id` or `company_ids`) with no declaration:
    /// a refusal for generation, enforcement and migration, never unrestricted.
    Undeclared,
}

/// Why company access does not apply to a model that declares nothing.
///
/// Exhaustive for the same reason as [`CompanyAccessState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotApplicable {
    /// An abstract model: its declaration is checked where a concrete model inherits it.
    Abstract,
    /// `res.company` itself, which keeps its own policy and takes no declaration.
    CompanyModel,
    /// A concrete model with no company field or qualifying required stored company-scoped link,
    /// and no declaration. Ownership is never inferred from a relational link.
    CompanyLess,
}

/// A model's validated company access (design §3.5): the payload of
/// [`CompanyAccessState::Declared`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompanyAccess {
    restriction: Restriction,
    declaration: CompanyAccessDecl,
    source: DeclSource,
}

impl CompanyAccess {
    /// What the declaration resolved to.
    pub const fn restriction(&self) -> Restriction {
        self.restriction
    }

    /// The effective declaration as written (for messages: [`CompanyAccessDecl::spelling`]).
    pub const fn declaration(&self) -> CompanyAccessDecl {
        self.declaration
    }

    /// Where the effective declaration was made.
    pub const fn source(&self) -> DeclSource {
        self.source
    }

    /// True when a restriction is generated from it (all but `unrestricted`).
    pub const fn generates(&self) -> bool {
        !matches!(self.restriction, Restriction::Unrestricted { .. })
    }

    /// The fields a write must change to move a record out of the restriction: the company field,
    /// the escape field when the declaration has one, and the owner edge, in that order.
    pub fn anchors(&self) -> impl Iterator<Item = FieldId> + use<> {
        let (first, escape, second) = match self.restriction {
            Restriction::Direct(d) => (Some(d.field), d.escape, None),
            Restriction::Owner(o) => (Some(o.edge), None, None),
            Restriction::DirectAndOwner(d, o) => (Some(d.field), d.escape, Some(o.edge)),
            Restriction::Unrestricted { .. } => (None, None, None),
        };
        first.into_iter().chain(escape).chain(second)
    }
}

impl Registry {
    /// Concrete company-scoped models (`company_id` or `company_ids`) that declare no
    /// `company_access`, in model order. Successful builds refuse these gaps.
    ///
    /// The state alone decides: `undeclared_state` gives [`CompanyAccessState::Undeclared`] only
    /// to a concrete model whose scope is `Single` or `Many`.
    pub fn company_access_gaps(&self) -> impl Iterator<Item = &ModelSpec> {
        self.models
            .iter()
            .filter(|m| matches!(m.company_access, CompanyAccessState::Undeclared))
    }
}

// ------------------------------------------------------------------------------------------------
// errors

/// Why an owner edge is not a stored column of the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EdgeStorage {
    /// Computed or related without `store`.
    NotStored,
    /// `company_dependent`.
    CompanyDependent,
    /// A copy of a field of an `_inherits` parent.
    Inherited {
        /// The parent model.
        parent: &'static str,
    },
}

/// Why the escape field of a `direct(…, escape = …)` declaration is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EscapeGap {
    /// The model has no such field.
    Unknown,
    /// The field is of another type (its type name).
    NotBoolean(&'static str),
    /// The field has no column of its own: computed or related without `store`,
    /// `company_dependent`, or a copy of an `_inherits` parent's field.
    NotStored,
    /// The field is not required, so its column may hold NULL.
    Optional,
}

/// Why the owner at the end of a hop does not restrict by company.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OwnerGap {
    /// It declares `unrestricted`.
    Unrestricted,
    /// It is company-scoped but declares nothing.
    Undeclared,
    /// It has neither a company field nor an owner.
    CompanyLess,
    /// It is an abstract model: it has no records, so no owner record can admit one.
    Abstract,
}

/// A `company_access` declaration the registry refuses (design §5). Converts into
/// [`erp_fields::Error`] of kind `Programming` with the same message and itself as source.
///
/// ```
/// use std::error::Error as _;
/// use erp_fields::ErrorKind;
/// use erp_registry::{
///     AddonDescriptor, CompanyAccessDecl, CompanyAccessError, ModelDecl, RegistryBuilder,
/// };
///
/// static SHOP: AddonDescriptor = AddonDescriptor { name: "shop", depends: &["base"], exact_only: false };
/// static COMPANY: ModelDecl = ModelDecl::new("shop", "res.company")
///     .company_access(CompanyAccessDecl::unrestricted("the company model"));
///
/// let err = RegistryBuilder::new([&SHOP])
///     .linked(false)
///     .with_decls([&COMPANY])
///     .build()
///     .unwrap_err();
/// assert_eq!(err.kind(), ErrorKind::Programming);
/// assert_eq!(
///     err.message(),
///     "res.company: company_access cannot be declared on the company model, which keeps its \
///      base access policy (declared by model definition 'res.company' of addon 'shop')"
/// );
/// let typed = err.source().unwrap().downcast_ref::<CompanyAccessError>().unwrap();
/// assert_eq!(typed.model(), "res.company");
/// assert_eq!(typed.declared_by().addon(), "shop");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompanyAccessError {
    /// A concrete company-scoped model has no access declaration.
    Undeclared {
        /// The model.
        model: &'static str,
        /// The company scope's field.
        scope_field: &'static str,
        /// The definition that created the model.
        declared_by: DeclSource,
    },
    /// A company-less child has a required link to a company-scoped model but no decision.
    UndeclaredChild {
        /// The child model.
        model: &'static str,
        /// The required stored relation.
        edge: &'static str,
        /// The company-scoped model reached by the relation.
        owner: &'static str,
        /// The definition that created the child.
        declared_by: DeclSource,
    },
    /// A direct part on a model without company scope.
    NotCompanyScoped {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The company field it names.
        field: &'static str,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// A direct part naming another field than the model's company field.
    WrongCompanyField {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The field it names.
        field: &'static str,
        /// The field of the model's company scope.
        scope_field: &'static str,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// Any declaration on `res.company`.
    OnCompanyModel {
        /// The model (`res.company`).
        model: &'static str,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// `branch = "ancestor"` while `res.company` keeps no hierarchy.
    AncestorWithoutHierarchy {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// `parent(…)` alone on a model with its own company field.
    OwnerWithOwnCompany {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The owner edge.
        edge: &'static str,
        /// The model's company field.
        scope_field: &'static str,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// The owner edge is not a field of the model.
    UnknownEdge {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The owner edge.
        edge: &'static str,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// The owner edge is not a many2one.
    EdgeNotMany2one {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The owner edge.
        edge: &'static str,
        /// Its type name.
        field_type: &'static str,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// The owner edge has no column of its own.
    EdgeNotStored {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The owner edge.
        edge: &'static str,
        /// Why.
        why: EdgeStorage,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// The owner edge is not required.
    EdgeOptional {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The owner edge.
        edge: &'static str,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// The owner edge links to `res.company`.
    EdgeToCompany {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The owner edge.
        edge: &'static str,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// The owner edge links to a model the registry does not have (defensive: the composer
    /// refuses unknown comodels before this pass runs).
    UnknownOwner {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The owner edge.
        edge: &'static str,
        /// The comodel it names.
        owner: String,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// The escape field of a direct declaration is unusable.
    BadEscape {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The escape field it names.
        escape: &'static str,
        /// Why it is refused.
        why: EscapeGap,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// A hop of an owner chain reaches a model that does not restrict by company.
    OwnerNotRestricted {
        /// The model of the hop.
        model: &'static str,
        /// Its declaration's spelling.
        spelling: String,
        /// The owner reached.
        owner: &'static str,
        /// The edge of the hop.
        edge: &'static str,
        /// Why the owner does not restrict.
        why: OwnerGap,
        /// The declaring definition of the hop's model.
        declared_by: DeclSource,
    },
    /// The owners loop back to a model of the chain.
    OwnerCycle {
        /// The first model of the cycle.
        model: &'static str,
        /// `(model, edge)` of every hop of the cycle, from `model`.
        cycle: Vec<(&'static str, &'static str)>,
        /// The declaring definition of `model`.
        declared_by: DeclSource,
    },
    /// The chain has more than [`MAX_OWNER_DEPTH`] edges.
    OwnerChainTooDeep {
        /// The model the chain starts at.
        model: &'static str,
        /// `(model, edge)` of every hop followed.
        path: Vec<(&'static str, &'static str)>,
        /// The model the last hop reached.
        end: &'static str,
        /// The declaring definition of `model`.
        declared_by: DeclSource,
    },
    /// `unrestricted` on a model without company scope or a qualifying required link.
    UnrestrictedWithoutCompany {
        /// The model.
        model: &'static str,
        /// The declaration's spelling.
        spelling: String,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// The reason of `unrestricted` is blank (empty or only whitespace) or too long.
    UnrestrictedReason {
        /// The model.
        model: &'static str,
        /// The reason's length in bytes: over [`MAX_UNRESTRICTED_REASON_BYTES`] when it is too
        /// long, otherwise the reason is blank (the length is checked first).
        bytes: usize,
        /// The declaring definition.
        declared_by: DeclSource,
    },
    /// A newer declaration is wider than the one before it.
    Widened {
        /// The model.
        model: &'static str,
        /// The newer declaration's spelling.
        new: String,
        /// Its definition.
        declared_by: DeclSource,
        /// The older declaration's spelling.
        old: String,
        /// Its definition.
        older_declared_by: DeclSource,
    },
    /// Two declarations name different owner edges.
    TwoOwners {
        /// The model.
        model: &'static str,
        /// The newer edge.
        edge_new: &'static str,
        /// The newer definition.
        declared_by: DeclSource,
        /// The older edge.
        edge_old: &'static str,
        /// The older definition.
        older_declared_by: DeclSource,
    },
}

impl CompanyAccessError {
    /// The error kind it converts to (always `Programming`: a declaration is code or addon data
    /// the registry cannot use).
    pub const fn kind(&self) -> ErrorKind {
        ErrorKind::Programming
    }

    /// The model the error is reported on.
    pub const fn model(&self) -> &'static str {
        match self {
            Self::Undeclared { model, .. }
            | Self::UndeclaredChild { model, .. }
            | Self::NotCompanyScoped { model, .. }
            | Self::WrongCompanyField { model, .. }
            | Self::OnCompanyModel { model, .. }
            | Self::AncestorWithoutHierarchy { model, .. }
            | Self::OwnerWithOwnCompany { model, .. }
            | Self::UnknownEdge { model, .. }
            | Self::EdgeNotMany2one { model, .. }
            | Self::EdgeNotStored { model, .. }
            | Self::EdgeOptional { model, .. }
            | Self::EdgeToCompany { model, .. }
            | Self::BadEscape { model, .. }
            | Self::UnknownOwner { model, .. }
            | Self::OwnerNotRestricted { model, .. }
            | Self::OwnerCycle { model, .. }
            | Self::OwnerChainTooDeep { model, .. }
            | Self::UnrestrictedWithoutCompany { model, .. }
            | Self::UnrestrictedReason { model, .. }
            | Self::Widened { model, .. }
            | Self::TwoOwners { model, .. } => model,
        }
    }

    /// The definition whose declaration is refused (the newer one for `Widened` and
    /// `TwoOwners`). Not named `source`, which would hide [`std::error::Error::source`].
    pub const fn declared_by(&self) -> DeclSource {
        match self {
            Self::Undeclared { declared_by, .. }
            | Self::UndeclaredChild { declared_by, .. }
            | Self::NotCompanyScoped { declared_by, .. }
            | Self::WrongCompanyField { declared_by, .. }
            | Self::OnCompanyModel { declared_by, .. }
            | Self::AncestorWithoutHierarchy { declared_by, .. }
            | Self::OwnerWithOwnCompany { declared_by, .. }
            | Self::UnknownEdge { declared_by, .. }
            | Self::EdgeNotMany2one { declared_by, .. }
            | Self::EdgeNotStored { declared_by, .. }
            | Self::EdgeOptional { declared_by, .. }
            | Self::EdgeToCompany { declared_by, .. }
            | Self::BadEscape { declared_by, .. }
            | Self::UnknownOwner { declared_by, .. }
            | Self::OwnerNotRestricted { declared_by, .. }
            | Self::OwnerCycle { declared_by, .. }
            | Self::OwnerChainTooDeep { declared_by, .. }
            | Self::UnrestrictedWithoutCompany { declared_by, .. }
            | Self::UnrestrictedReason { declared_by, .. }
            | Self::Widened { declared_by, .. }
            | Self::TwoOwners { declared_by, .. } => *declared_by,
        }
    }
}

fn write_hops(f: &mut fmt::Formatter<'_>, hops: &[(&str, &str)], end: &str) -> fmt::Result {
    for (model, edge) in hops {
        write!(f, "{model}.{edge} -> ")?;
    }
    f.write_str(end)
}

impl fmt::Display for CompanyAccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undeclared {
                model,
                scope_field,
                declared_by,
            } => write!(
                f,
                "{model}: belongs to companies through '{scope_field}' but declares no company_access; declare direct(field = \"{scope_field}\", branch = \"exact\" or \"ancestor\", empty = \"allow\" or \"deny\"), add parent = \"<owner>\" when another record owns it, or unrestricted(reason = \"…\") (model created by {declared_by})"
            ),
            Self::UndeclaredChild {
                model,
                edge,
                owner,
                declared_by,
            } => write!(
                f,
                "{model}: required stored link '{edge}' points to company-scoped {owner} but declares no company_access; declare parent(edge = \"{edge}\") or unrestricted(reason = \"…\") (model created by {declared_by})"
            ),
            Self::NotCompanyScoped {
                model,
                spelling,
                field,
                declared_by,
            } => write!(
                f,
                "{model}: company_access {spelling} names the company field '{field}', but \
                 {model} has no company_id or company_ids field linked to res.company (declared \
                 by {declared_by})"
            ),
            Self::WrongCompanyField {
                model,
                spelling,
                field,
                scope_field,
                declared_by,
            } => write!(
                f,
                "{model}: company_access {spelling} names the company field '{field}', but \
                 {model} belongs to companies through '{scope_field}' (declared by {declared_by})"
            ),
            Self::OnCompanyModel { model, declared_by } => write!(
                f,
                "{model}: company_access cannot be declared on the company model, which \
                 keeps its base access policy (declared by {declared_by})"
            ),
            Self::AncestorWithoutHierarchy {
                model,
                spelling,
                declared_by,
            } => write!(
                f,
                "{model}: company_access {spelling} uses branch \"ancestor\", which needs \
                 res.company to keep its hierarchy (a parent field and _parent_store); use branch \
                 \"exact\" or declare parent_store on res.company (declared by {declared_by})"
            ),
            Self::OwnerWithOwnCompany {
                model,
                spelling,
                edge,
                scope_field,
                declared_by,
            } => write!(
                f,
                "{model}: company_access {spelling} names only the owner '{edge}', but {model} \
                 has its own company field '{scope_field}': declare its direct restriction too, \
                 direct(field = \"{scope_field}\", branch = …, empty = …, parent = \"{edge}\") \
                 (declared by {declared_by})"
            ),
            Self::UnknownEdge {
                model,
                spelling,
                edge,
                declared_by,
            } => write!(
                f,
                "{model}: company_access {spelling} names the owner '{edge}', which is not a \
                 field of {model} (declared by {declared_by})"
            ),
            Self::EdgeNotMany2one {
                model,
                spelling,
                edge,
                field_type,
                declared_by,
            } => write!(
                f,
                "{model}: company_access {spelling} names the owner '{edge}', but \
                 {model}.{edge} is a {field_type}, not a many2one (declared by {declared_by})"
            ),
            Self::EdgeNotStored {
                model,
                spelling,
                edge,
                why,
                declared_by,
            } => {
                write!(
                    f,
                    "{model}: company_access {spelling} names the owner '{edge}', but \
                     {model}.{edge} "
                )?;
                match why {
                    EdgeStorage::NotStored => f.write_str("is not stored")?,
                    EdgeStorage::CompanyDependent => f.write_str("is company-dependent")?,
                    EdgeStorage::Inherited { parent } => write!(
                        f,
                        "is inherited from {parent} through _inherits; declare the restriction \
                         on {parent}"
                    )?,
                }
                write!(f, " (declared by {declared_by})")
            }
            Self::EdgeOptional {
                model,
                spelling,
                edge,
                declared_by,
            } => write!(
                f,
                "{model}: company_access {spelling} names the owner '{edge}', but \
                 {model}.{edge} is optional: an owner edge is required (required = true), so \
                 every record has an owner (declared by {declared_by})"
            ),
            Self::BadEscape {
                model,
                spelling,
                escape,
                why,
                declared_by,
            } => {
                write!(
                    f,
                    "{model}: company_access {spelling} names the escape '{escape}', but "
                )?;
                match why {
                    EscapeGap::Unknown => write!(f, "{model} has no field '{escape}'"),
                    EscapeGap::NotBoolean(ty) => write!(
                        f,
                        "{model}.{escape} is a {ty} field: an escape must be a boolean"
                    ),
                    EscapeGap::NotStored => write!(
                        f,
                        "{model}.{escape} has no column of its own (computed or related without \
                         store, company_dependent, or inherited): the rule runs in SQL against \
                         the column"
                    ),
                    EscapeGap::Optional => write!(
                        f,
                        "{model}.{escape} is optional: an escape must be required (required = \
                         true), because a negative boolean test also matches a NULL column and \
                         would admit the row"
                    ),
                }?;
                write!(f, " (declared by {declared_by})")
            }
            Self::EdgeToCompany {
                model,
                spelling,
                edge,
                declared_by,
            } => write!(
                f,
                "{model}: company_access {spelling} names the owner '{edge}', which links to \
                 res.company: restrict by the company field instead, direct(field = \
                 \"company_id\" or \"company_ids\", …) (declared by {declared_by})"
            ),
            Self::UnknownOwner {
                model,
                spelling,
                edge,
                owner,
                declared_by,
            } => write!(
                f,
                "{model}: company_access {spelling} names the owner '{edge}', whose model \
                 {owner} is not in the registry (declared by {declared_by})"
            ),
            Self::OwnerNotRestricted {
                model,
                spelling,
                owner,
                edge,
                why,
                declared_by,
            } => {
                let why = match why {
                    OwnerGap::Unrestricted => "is declared unrestricted",
                    OwnerGap::Undeclared => "declares no company_access",
                    OwnerGap::CompanyLess => "has no company field and no owner",
                    OwnerGap::Abstract => "is abstract and has no records",
                };
                write!(
                    f,
                    "{model}: company_access {spelling} inherits the company restriction of \
                     {owner} through '{edge}', but {owner} {why}: an owner chain must end in a \
                     direct company restriction (declared by {declared_by})"
                )
            }
            Self::OwnerCycle {
                model,
                cycle,
                declared_by,
            } => {
                write!(f, "{model}: the owners of company_access form a cycle: ")?;
                write_hops(f, cycle, model)?;
                write!(f, " (declared by {declared_by})")
            }
            Self::OwnerChainTooDeep {
                model,
                path,
                end,
                declared_by,
            } => {
                write!(
                    f,
                    "{model}: the owners of company_access are more than {MAX_OWNER_DEPTH} edges \
                     deep: "
                )?;
                write_hops(f, path, end)?;
                write!(f, " (declared by {declared_by})")
            }
            Self::UnrestrictedWithoutCompany {
                model,
                spelling,
                declared_by,
            } => write!(
                f,
                "{model}: company_access {spelling} is declared, but {model} has neither a \
                 company field nor a required stored link to a company-scoped model; no \
                 declaration is needed (declared by {declared_by})"
            ),
            Self::UnrestrictedReason {
                model,
                bytes,
                declared_by,
            } => write!(
                f,
                "{model}: company_access {} (declared by {declared_by})",
                ReasonRefusal(*bytes)
            ),
            Self::Widened {
                model,
                new,
                declared_by,
                old,
                older_declared_by,
            } => write!(
                f,
                "{model}: company_access {new} (declared by {declared_by}) is wider than {old} \
                 (declared by {older_declared_by}): a later declaration may only narrow a \
                 model's company restriction"
            ),
            Self::TwoOwners {
                model,
                edge_new,
                declared_by,
                edge_old,
                older_declared_by,
            } => write!(
                f,
                "{model}: company_access names the owner '{edge_new}' (declared by {declared_by}), \
                 but {older_declared_by} names the owner '{edge_old}': a record has one owner"
            ),
        }
    }
}

impl std::error::Error for CompanyAccessError {}

impl From<CompanyAccessError> for Error {
    fn from(e: CompanyAccessError) -> Self {
        Error::new(e.kind(), e.to_string()).with_source(e)
    }
}

impl From<Box<CompanyAccessError>> for Error {
    fn from(e: Box<CompanyAccessError>) -> Self {
        Self::from(*e)
    }
}

// ------------------------------------------------------------------------------------------------
// the pass

/// What the internal checks return: the error boxed, since a refusal is rare and the error is
/// large (clippy `result_large_err`); [`finish`]'s caller converts it into [`Error`].
type Refused<T> = std::result::Result<T, Box<CompanyAccessError>>;

/// A concrete model's effective declaration, checked on its own (phase 1).
#[derive(Clone, Copy)]
struct Local {
    access: CompanyAccess,
    /// The declaring definition, for messages.
    def: &'static ModelDecl,
    /// `(edge name, owner)` of an owner restriction.
    owner: Option<(&'static str, ModelId)>,
}

/// Validate every concrete model's `company_access` and set [`ModelSpec::company_access`]
/// (called by the composer after `company::finish`, so scopes are known). Pure.
///
/// # Errors
///
/// The first [`CompanyAccessError`], in model order: composition errors and the model's own
/// checks for every model first, owner chains after.
pub(crate) fn finish(
    models: &mut [ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
) -> Result<()> {
    let hierarchy = by_name.get(COMPANY_MODEL).is_some_and(|id| {
        // invariant: `by_name` maps to ids of `models`
        let company = &models[id.index()];
        company.parent_path.is_some() && company.parent_name.is_some()
    });
    let mut local = Vec::with_capacity(models.len());
    for model in models.iter() {
        local.push(resolve(model, models, by_name, hierarchy)?);
    }
    for (i, l) in local.iter().enumerate() {
        if l.is_some_and(|l| l.owner.is_some()) {
            check_chain(i, models, &local)?;
        }
    }
    for (model, l) in models.iter().zip(&local) {
        if l.is_none() && !model.is_abstract() {
            let creator = model
                .mro
                .iter()
                .find(|d| d.name == model.name && !d.is_extension())
                .ok_or_else(|| {
                    Error::programming(format!(
                        "model '{}': company access validation needs a creating definition",
                        model.name
                    ))
                })?;
            if let Some((field, _)) = scope_of(model) {
                return Err(CompanyAccessError::Undeclared {
                    model: model.name,
                    scope_field: model.fields[field.index()].name,
                    declared_by: DeclSource::of(creator),
                }
                .into());
            }
            if let Some((edge, owner)) = scoped_owner_edge(model, models, by_name) {
                return Err(CompanyAccessError::UndeclaredChild {
                    model: model.name,
                    edge,
                    owner,
                    declared_by: DeclSource::of(creator),
                }
                .into());
            }
        }
    }
    for (model, l) in models.iter_mut().zip(local) {
        model.company_access = match l {
            Some(l) => CompanyAccessState::Declared(l.access),
            None => undeclared_state(model),
        };
    }
    Ok(())
}

/// The state of a model with no effective declaration: not applicable for abstract models,
/// `res.company` and models without a company field, [`CompanyAccessState::Undeclared`] for a
/// concrete company-scoped one (never a state that reads as open).
fn undeclared_state(model: &ModelSpec) -> CompanyAccessState {
    if model.is_abstract() {
        return CompanyAccessState::NotApplicable(NotApplicable::Abstract);
    }
    match model.company_scope {
        Some(CompanyScope::Single { .. } | CompanyScope::Many { .. }) => {
            CompanyAccessState::Undeclared
        }
        Some(CompanyScope::Itself) => {
            CompanyAccessState::NotApplicable(NotApplicable::CompanyModel)
        }
        None => CompanyAccessState::NotApplicable(NotApplicable::CompanyLess),
    }
}

/// The scope's field and cardinality, `None` when the model has no company field. `res.company`
/// (`Itself`) is refused by `OnCompanyModel` before any check asks for its scope, so `Itself`
/// only reaches here for an owner hop, where it counts as "no company field".
fn scope_of(model: &ModelSpec) -> Option<(FieldId, CompanyCardinality)> {
    match model.company_scope {
        Some(CompanyScope::Single { field, .. }) => Some((field, CompanyCardinality::Single)),
        Some(CompanyScope::Many { field, .. }) => Some((field, CompanyCardinality::Many)),
        Some(CompanyScope::Itself) | None => None,
    }
}

/// A required stored relation to a directly company-scoped model permits an explicit
/// `unrestricted` decision. The link alone never grants or infers access.
fn scoped_owner_edge(
    model: &ModelSpec,
    models: &[ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
) -> Option<(&'static str, &'static str)> {
    model.fields.iter().find_map(|field| {
        if !field.required || !field.store || field.company_dependent || field.inherited.is_some() {
            return None;
        }
        let FieldType::Many2one { comodel, .. } = &field.ty else {
            return None;
        };
        let target = &models[by_name.get(comodel.as_str())?.index()];
        scope_of(target).map(|_| (field.name, target.name))
    })
}

/// The checks of §5 that apply to one declaration alone, whichever definition of the MRO made it,
/// on abstract models too: today the bounds of an `unrestricted` reason (not blank, at most
/// [`MAX_UNRESTRICTED_REASON_BYTES`]).
fn check_alone(model: &ModelSpec, decl: CompanyAccessDecl, def: &'static ModelDecl) -> Refused<()> {
    if let CompanyAccessDecl::Unrestricted(u) = decl
        && let Some(bytes) = refused_reason(u.reason)
    {
        return Err(Box::new(CompanyAccessError::UnrestrictedReason {
            model: model.name,
            bytes,
            declared_by: DeclSource::of(def),
        }));
    }
    Ok(())
}

/// Compose the declarations of one model through its MRO and check the effective one (phase 1).
fn resolve(
    model: &ModelSpec,
    models: &[ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
    hierarchy: bool,
) -> Refused<Option<Local>> {
    // oldest first (the MRO is newest first)
    let declared: Vec<(CompanyAccessDecl, &'static ModelDecl)> = model
        .mro
        .iter()
        .rev()
        .filter_map(|d| d.company_access.map(|a| (a, *d)))
        .collect();
    // abstract declarations are checked where they are made concrete (A1-D3); only the checks
    // of a declaration alone run here too, so a mixin nothing makes concrete is still bounded
    // (review L1)
    if model.is_abstract() {
        for &(d, d_def) in &declared {
            check_alone(model, d, d_def)?;
        }
        return Ok(None);
    }
    let Some(&(decl, def)) = declared.last() else {
        return Ok(None);
    };
    // whatever the declarations say, the company model takes none
    if model.name == COMPANY_MODEL {
        return Err(Box::new(CompanyAccessError::OnCompanyModel {
            model: model.name,
            declared_by: DeclSource::of(def),
        }));
    }
    for &(d, d_def) in &declared {
        check_alone(model, d, d_def)?;
    }
    for i in 1..declared.len() {
        let (old, old_def) = declared[i - 1];
        let (new, new_def) = declared[i];
        if let (Some(edge_old), Some(edge_new)) = (old.owner_edge(), new.owner_edge())
            && edge_old != edge_new
        {
            return Err(Box::new(CompanyAccessError::TwoOwners {
                model: model.name,
                edge_new,
                declared_by: DeclSource::of(new_def),
                edge_old,
                older_declared_by: DeclSource::of(old_def),
            }));
        }
        if !new.within(&old) {
            return Err(Box::new(CompanyAccessError::Widened {
                model: model.name,
                new: new.spelling(),
                declared_by: DeclSource::of(new_def),
                old: old.spelling(),
                older_declared_by: DeclSource::of(old_def),
            }));
        }
    }
    let check = Check {
        model,
        models,
        decl,
        def,
        by_name,
        hierarchy,
    };
    let (restriction, owner) = match decl {
        // the reason was checked with every declaration above
        CompanyAccessDecl::Unrestricted(u) => {
            if scope_of(model).is_none() && scoped_owner_edge(model, models, by_name).is_none() {
                return Err(Box::new(CompanyAccessError::UnrestrictedWithoutCompany {
                    model: model.name,
                    spelling: decl.spelling(),
                    declared_by: DeclSource::of(def),
                }));
            }
            (Restriction::Unrestricted { reason: u.reason }, None)
        }
        CompanyAccessDecl::Direct(d) => (Restriction::Direct(check.direct(d)?), None),
        CompanyAccessDecl::Parent(p) => {
            if let Some((field, _)) = scope_of(model) {
                return Err(Box::new(CompanyAccessError::OwnerWithOwnCompany {
                    model: model.name,
                    spelling: decl.spelling(),
                    edge: p.edge,
                    scope_field: model.field_by_id(field).name,
                    declared_by: DeclSource::of(def),
                }));
            }
            let o = check.edge(p.edge)?;
            (Restriction::Owner(o), Some((p.edge, o.owner)))
        }
        CompanyAccessDecl::DirectAndParent(d, p) => {
            let d = check.direct(d)?;
            let o = check.edge(p.edge)?;
            (Restriction::DirectAndOwner(d, o), Some((p.edge, o.owner)))
        }
    };
    Ok(Some(Local {
        access: CompanyAccess {
            restriction,
            declaration: decl,
            source: DeclSource::of(def),
        },
        def,
        owner,
    }))
}

/// The model-local checks of one effective declaration.
struct Check<'a> {
    model: &'a ModelSpec,
    models: &'a [ModelSpec],
    decl: CompanyAccessDecl,
    def: &'static ModelDecl,
    by_name: &'a FxHashMap<&'static str, ModelId>,
    hierarchy: bool,
}

impl Check<'_> {
    fn direct(&self, d: DirectDecl) -> Refused<DirectRestriction> {
        let model = self.model;
        let Some((field, cardinality)) = scope_of(model) else {
            return Err(Box::new(CompanyAccessError::NotCompanyScoped {
                model: model.name,
                spelling: self.decl.spelling(),
                field: d.field,
                declared_by: DeclSource::of(self.def),
            }));
        };
        let scope_field = model.field_by_id(field).name;
        if scope_field != d.field {
            return Err(Box::new(CompanyAccessError::WrongCompanyField {
                model: model.name,
                spelling: self.decl.spelling(),
                field: d.field,
                scope_field,
                declared_by: DeclSource::of(self.def),
            }));
        }
        if d.branch == CompanyBranch::Ancestor && !self.hierarchy {
            return Err(Box::new(CompanyAccessError::AncestorWithoutHierarchy {
                model: model.name,
                spelling: self.decl.spelling(),
                declared_by: DeclSource::of(self.def),
            }));
        }
        let escape = d.escape.map(|name| self.escape(d, name)).transpose()?;
        Ok(DirectRestriction {
            field,
            cardinality,
            branch: d.branch,
            empty: d.empty,
            escape,
        })
    }

    /// The escape field: a stored, required boolean with a column of its own. Required, because
    /// the generated test is a negative boolean one, which also matches a NULL column.
    fn escape(&self, d: DirectDecl, name: &'static str) -> Refused<FieldId> {
        let model = self.model;
        let refuse = |why: EscapeGap| {
            Box::new(CompanyAccessError::BadEscape {
                model: model.name,
                spelling: self.decl.spelling(),
                escape: name,
                why,
                declared_by: DeclSource::of(self.def),
            })
        };
        let Some(f) = model.field(name) else {
            return Err(refuse(EscapeGap::Unknown));
        };
        if !matches!(f.ty, FieldType::Boolean) {
            return Err(refuse(EscapeGap::NotBoolean(f.ty.type_name())));
        }
        if f.inherited.is_some() || f.company_dependent || !f.store {
            return Err(refuse(EscapeGap::NotStored));
        }
        if !f.required {
            return Err(refuse(EscapeGap::Optional));
        }
        debug_assert_ne!(name, d.field, "a boolean is never the company field");
        Ok(f.id)
    }

    fn edge(&self, edge: &'static str) -> Refused<OwnerRestriction> {
        let model = self.model;
        let Some(f) = model.field(edge) else {
            return Err(Box::new(CompanyAccessError::UnknownEdge {
                model: model.name,
                spelling: self.decl.spelling(),
                edge,
                declared_by: DeclSource::of(self.def),
            }));
        };
        let FieldType::Many2one { comodel, .. } = &f.ty else {
            return Err(Box::new(CompanyAccessError::EdgeNotMany2one {
                model: model.name,
                spelling: self.decl.spelling(),
                edge,
                field_type: f.ty.type_name(),
                declared_by: DeclSource::of(self.def),
            }));
        };
        if comodel == COMPANY_MODEL {
            return Err(Box::new(CompanyAccessError::EdgeToCompany {
                model: model.name,
                spelling: self.decl.spelling(),
                edge,
                declared_by: DeclSource::of(self.def),
            }));
        }
        let storage = if let Some(inherited) = f.inherited {
            Some(EdgeStorage::Inherited {
                // invariant: `inherited.parent` is an id of this registry's models
                parent: self.models[inherited.parent.index()].name,
            })
        } else if f.company_dependent {
            Some(EdgeStorage::CompanyDependent)
        } else if !f.store {
            Some(EdgeStorage::NotStored)
        } else {
            None
        };
        if let Some(why) = storage {
            return Err(Box::new(CompanyAccessError::EdgeNotStored {
                model: model.name,
                spelling: self.decl.spelling(),
                edge,
                why,
                declared_by: DeclSource::of(self.def),
            }));
        }
        if !f.required {
            return Err(Box::new(CompanyAccessError::EdgeOptional {
                model: model.name,
                spelling: self.decl.spelling(),
                edge,
                declared_by: DeclSource::of(self.def),
            }));
        }
        let Some(&owner) = self.by_name.get(comodel.as_str()) else {
            return Err(Box::new(CompanyAccessError::UnknownOwner {
                model: model.name,
                spelling: self.decl.spelling(),
                edge,
                owner: comodel.clone(),
                declared_by: DeclSource::of(self.def),
            }));
        };
        Ok(OwnerRestriction { edge: f.id, owner })
    }
}

/// One hop of an owner chain: the model, its owner edge, the declaration that names it.
#[derive(Clone, Copy)]
struct Hop {
    model: usize,
    edge: &'static str,
    local: Local,
}

/// Follow the owners from model `start` until a model restricted only directly (phase 2).
fn check_chain(start: usize, models: &[ModelSpec], local: &[Option<Local>]) -> Refused<()> {
    // invariant: `local` has one entry per model, and every hop's model and owner are ids of
    // `models`
    let named = |hops: &[Hop]| -> Vec<(&'static str, &'static str)> {
        hops.iter()
            .map(|h| (models[h.model].name, h.edge))
            .collect()
    };
    let mut path: Vec<Hop> = Vec::with_capacity(MAX_OWNER_DEPTH + 1);
    let mut current = start;
    // a model without an owner ends the walk (the caller starts only at models with one)
    while let Some(here) = local[current]
        && let Some((edge, owner)) = here.owner
    {
        path.push(Hop {
            model: current,
            edge,
            local: here,
        });
        let next = owner.index();
        if let Some(pos) = path.iter().position(|h| h.model == next) {
            let first = path[pos];
            return Err(Box::new(CompanyAccessError::OwnerCycle {
                model: models[first.model].name,
                cycle: named(&path[pos..]),
                declared_by: DeclSource::of(first.local.def),
            }));
        }
        if path.len() > MAX_OWNER_DEPTH {
            return Err(Box::new(CompanyAccessError::OwnerChainTooDeep {
                model: models[start].name,
                path: named(&path),
                end: models[next].name,
                declared_by: DeclSource::of(path[0].local.def),
            }));
        }
        let gap = match &local[next] {
            None if models[next].is_abstract() => Some(OwnerGap::Abstract),
            None => Some(if scope_of(&models[next]).is_some() {
                OwnerGap::Undeclared
            } else {
                OwnerGap::CompanyLess
            }),
            Some(l) => match l.access.restriction {
                Restriction::Unrestricted { .. } => Some(OwnerGap::Unrestricted),
                Restriction::Direct(_) => return Ok(()),
                Restriction::Owner(_) | Restriction::DirectAndOwner(..) => None,
            },
        };
        if let Some(why) = gap {
            // the hop that is wrong, whichever model the walk started from
            return Err(Box::new(CompanyAccessError::OwnerNotRestricted {
                model: models[current].name,
                spelling: here.access.declaration.spelling(),
                owner: models[next].name,
                edge,
                why,
                declared_by: DeclSource::of(here.def),
            }));
        }
        current = next;
    }
    Ok(())
}
