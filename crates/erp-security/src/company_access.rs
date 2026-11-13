//! Generated company restrictions (amendment A1-D2/A1-D3; design,
//! §6.1): the access rows a model's `company_access`
//! declaration stands for, and the check of the rows found in a database against them.
//!
//! [`generate`] turns every declaration that restricts into plain rule text (the same text a
//! hand-written `ir.access` restriction row would carry, so the existing rule compiler, combiner
//! and explainer handle it), together with its **deny variant** (every `allow` replaced by `deny`
//! and each escape omitted,
//! for a reader whose validated active-company set is empty, design §4.3) and its row name. Each
//! text is compiled here; a text that does not compile or does not name the company is a bug of
//! this module and fails the call, never a silently weaker policy.
//!
//! [`verify`] compares the generated rows as read (active ones from the policy rows, archived ones
//! from the loader's separate list) with that expected set and lists every difference. Pure and
//! deterministic: no database, no clock.
//!
//! ```
//! use erp_registry::RegistryBuilder;
//! // a registry without company-scoped models generates nothing
//! let registry = RegistryBuilder::new([]).linked(false).build()?;
//! assert!(erp_security::company_access::generate(&registry)?.is_empty());
//! # Ok::<(), erp_fields::Error>(())
//! ```

use std::collections::BTreeMap;
use std::fmt;

use erp_fields::{Error, Result};
use erp_registry::{
    CompanyAccess, CompanyAccessState, CompanyBranch, CompanyCardinality, DeclSource,
    DirectRestriction, EmptyCompany, MAX_OWNER_DEPTH, ModelId, ModelSpec, OwnerRestriction,
    Registry, Restriction,
};

use erp_orm::GroupId;

use crate::rows::{RuleId, RuleRow};
use crate::rule;

/// The operations a generated restriction covers: all four.
pub const GENERATED_OPERATION: &str = "crud";

/// One model's generated company restriction (design §6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedRestriction {
    model: ModelId,
    model_name: &'static str,
    name: String,
    text: String,
    deny_text: String,
    spelling: String,
    source: DeclSource,
}

impl GeneratedRestriction {
    /// The restricted model.
    pub fn model(&self) -> ModelId {
        self.model
    }

    /// The restricted model's name.
    pub fn model_name(&self) -> &'static str {
        self.model_name
    }

    /// The row name (design §4.7): changes only when the declaration does.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The domain text of the row.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The text compiled for a reader with no active company: the same chain with every `allow`
    /// replaced by `deny` and each escape omitted (equal to [`Self::text`] only when the chain
    /// has neither `allow` nor an escape).
    pub fn deny_text(&self) -> &str {
        &self.deny_text
    }

    /// The declaration as written (`direct(field = "company_id", branch = "exact", empty =
    /// "deny")`), for messages.
    pub fn spelling(&self) -> &str {
        &self.spelling
    }

    /// Where the declaration was made.
    pub fn source(&self) -> DeclSource {
        self.source
    }
}

/// The generated restriction of every model whose company access generates, sorted by model name.
///
/// A concrete company-scoped model without a declaration (`CompanyAccessState::Undeclared`)
/// generates nothing and is not reported here: refusing it is the switch
/// (design §7.1, `Registry::company_access_gaps`), not this sub-slice.
///
/// # Errors
///
/// `Programming` for a generator bug: an owner chain deeper than [`MAX_OWNER_DEPTH`] or ending
/// in a model that does not restrict (the registry refuses both when it is built), a text the rule
/// compiler rejects, or a text that does not name the company (so scoped elevation could lift it,
/// design §4.6).
pub fn generate(registry: &Registry) -> Result<Vec<GeneratedRestriction>> {
    let mut out = Vec::new();
    for model in registry.models() {
        let CompanyAccessState::Declared(access) = model.company_access else {
            continue;
        };
        if !access.generates() {
            continue;
        }
        let text = list_text(registry, model, access, Variant::Declared, 0)?;
        let deny_text = list_text(registry, model, access, Variant::Deny, 0)?;
        for t in [&text, &deny_text] {
            check_compiles(registry, model, t, &access)?;
        }
        out.push(GeneratedRestriction {
            model: registry
                .model_id(model.name)
                .ok_or_else(|| Error::programming(format!("model '{}' has no id", model.name)))?,
            model_name: model.name,
            name: row_name(model, access)?,
            text,
            deny_text,
            spelling: access.declaration().spelling(),
            source: access.source(),
        });
    }
    out.sort_by(|a, b| a.model_name.cmp(b.model_name));
    Ok(out)
}

fn check_compiles(
    registry: &Registry,
    model: &ModelSpec,
    text: &str,
    access: &CompanyAccess,
) -> Result<()> {
    let compiled = rule::compile(registry, model, Some(text)).map_err(|e| {
        Error::programming(format!(
            "the generated company restriction of {} ({}, declared by {}) does not compile: {e}",
            model.name,
            access.declaration().spelling(),
            access.source()
        ))
    })?;
    if !compiled.mentions_company {
        return Err(Error::programming(format!(
            "the generated company restriction of {} ({}, declared by {}) does not name the \
             company",
            model.name,
            access.declaration().spelling(),
            access.source()
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    Declared,
    Deny,
}

/// The domain list text of a model: `[ … ]` around its prefix expression.
fn list_text(
    registry: &Registry,
    model: &ModelSpec,
    access: CompanyAccess,
    variant: Variant,
    depth: usize,
) -> Result<String> {
    Ok(format!(
        "[{}]",
        prefix_text(registry, model, access, variant, depth)?
    ))
}

/// The prefix expression of a model's restriction, without brackets.
fn prefix_text(
    registry: &Registry,
    model: &ModelSpec,
    access: CompanyAccess,
    variant: Variant,
    depth: usize,
) -> Result<String> {
    if depth > MAX_OWNER_DEPTH {
        return Err(Error::programming(format!(
            "the owner chain of {} is deeper than {MAX_OWNER_DEPTH} levels",
            model.name
        )));
    }
    match access.restriction() {
        Restriction::Direct(d) => direct_text(model, d, variant),
        Restriction::Owner(o) => owner_text(registry, model, o, variant, depth),
        Restriction::DirectAndOwner(d, o) => Ok(format!(
            "'&', {}, {}",
            direct_text(model, d, variant)?,
            owner_text(registry, model, o, variant, depth)?
        )),
        // `generates()` is false for it; callers skip it, so reaching here is a generator bug
        _ => Err(Error::programming(format!(
            "{} has no generated company restriction",
            model.name
        ))),
    }
}

/// One condition (or an `|` pair) on the model's company field.
fn direct_text(model: &ModelSpec, d: DirectRestriction, variant: Variant) -> Result<String> {
    let field = model.field_by_id(d.field()).name;
    let allow = d.empty() == EmptyCompany::Allow && variant == Variant::Declared;
    let operator = match d.branch() {
        CompanyBranch::Exact => "in",
        CompanyBranch::Ancestor => "parent_of",
        // a new branch mode must come with its generated text; fail loudly until it does
        other => {
            return Err(Error::programming(format!(
                "{}: no generated text for the branch mode {other:?}",
                model.name
            )));
        }
    };
    let company = match (d.cardinality(), d.branch(), allow) {
        (CompanyCardinality::Single, CompanyBranch::Exact, true) => {
            format!("('{field}', 'in', company_ids + [False])")
        }
        (_, _, true) => {
            format!("'|', ('{field}', '=', False), ('{field}', '{operator}', company_ids)")
        }
        (_, _, false) => format!("('{field}', '{operator}', company_ids)"),
    };
    // The escape only widens the declared variant. The deny variant (the empty active-company
    // set) admits nothing, so it keeps only the company test.
    //
    // The negative boolean test compiles to `IS NOT TRUE`, which would also admit NULL.
    // The registry's required-field check and the column's NOT NULL constraint keep NULL out.
    Ok(match d.escape() {
        Some(escape) if variant == Variant::Declared => format!(
            "'|', ('{}', '=', False), {company}",
            model.field_by_id(escape).name
        ),
        _ => company,
    })
}

/// The condition on the owner edge: the owner's whole text nested in `any`.
fn owner_text(
    registry: &Registry,
    model: &ModelSpec,
    o: OwnerRestriction,
    variant: Variant,
    depth: usize,
) -> Result<String> {
    let edge = model.field_by_id(o.edge()).name;
    let owner = registry.model_by_id(o.owner());
    let CompanyAccessState::Declared(owner_access) = owner.company_access else {
        return Err(Error::programming(format!(
            "{} is owned by {} through '{edge}', which declares no company access",
            model.name, owner.name
        )));
    };
    if !owner_access.generates() {
        return Err(Error::programming(format!(
            "{} is owned by {} through '{edge}', which is declared unrestricted",
            model.name, owner.name
        )));
    }
    let inner = list_text(registry, owner, owner_access, variant, depth + 1)?;
    Ok(format!("('{edge}', 'any', {inner})"))
}

/// The row name (design §4.7).
fn row_name(model: &ModelSpec, access: CompanyAccess) -> Result<String> {
    let direct = |d: DirectRestriction| {
        let base = format!(
            "{}, {}, {}",
            model.field_by_id(d.field()).name,
            d.branch().as_str(),
            d.empty().as_str()
        );
        match d.escape() {
            Some(escape) => format!("{base}, escape {}", model.field_by_id(escape).name),
            None => base,
        }
    };
    let edge = |o: OwnerRestriction| format!("through {}", model.field_by_id(o.edge()).name);
    let summary = match access.restriction() {
        Restriction::Direct(d) => direct(d),
        Restriction::Owner(o) => edge(o),
        Restriction::DirectAndOwner(d, o) => format!("{}; {}", direct(d), edge(o)),
        // `generate` renders the text first and refuses a restriction it cannot render
        _ => {
            return Err(Error::programming(format!(
                "{} has no row name for its company restriction",
                model.name
            )));
        }
    };
    Ok(format!("Company restriction of {} ({summary})", model.name))
}

// ------------------------------------------------------------------------------------------------
// rows as read

/// One `ir.access` row marked generated, as the loader read it (active or archived).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedRow {
    id: RuleId,
    model: Box<str>,
    name: Box<str>,
    active: bool,
    group: Option<GroupId>,
    operation: Box<str>,
    domain: Option<Box<str>>,
}

impl GeneratedRow {
    /// A generated row without group and domain.
    pub fn new(id: RuleId, model: &str, name: &str, active: bool, operation: &str) -> Self {
        Self {
            id,
            model: model.into(),
            name: name.into(),
            active,
            group: None,
            operation: operation.into(),
            domain: None,
        }
    }

    /// Give the row a group (a permission; always a finding for a generated row).
    #[must_use]
    pub fn with_group(mut self, group: GroupId) -> Self {
        self.group = Some(group);
        self
    }

    /// Set the domain text.
    #[must_use]
    pub fn with_domain(mut self, domain: &str) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// The active row `rule` as a generated row.
    pub fn from_active(rule: &RuleRow) -> Self {
        Self {
            id: rule.id(),
            model: rule.model().into(),
            name: rule.name().into(),
            active: true,
            group: rule.group(),
            operation: rule.operation().into(),
            domain: rule.domain().map(Into::into),
        }
    }

    pub(crate) fn archived(mut self) -> Self {
        self.active = false;
        self
    }

    /// The row id.
    pub fn id(&self) -> RuleId {
        self.id
    }

    /// The model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The row name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the row is active.
    pub fn active(&self) -> bool {
        self.active
    }

    /// The group, if the row was given one.
    pub fn group(&self) -> Option<GroupId> {
        self.group
    }

    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// The domain text as stored.
    pub fn domain(&self) -> Option<&str> {
        self.domain.as_deref()
    }
}

/// Which part of a generated row differs from what the declaration gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Changed {
    /// The domain text.
    Domain,
    /// The operation (must be `crud`).
    Operation,
    /// The row has a group (it must be a group-less restriction).
    Group,
}

impl Changed {
    /// The word messages use.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Domain => "domain",
            Self::Operation => "operation",
            Self::Group => "group",
        }
    }
}

impl fmt::Display for Changed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What is wrong with a model's generated row (design §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum GeneratedIssue {
    /// The model generates, but no row exists.
    Missing,
    /// The row is archived.
    Archived,
    /// The row differs from what the declaration gives.
    Changed {
        /// The part that differs.
        what: Changed,
    },
    /// More than one generated row for the model.
    Duplicate {
        /// How many rows.
        count: usize,
    },
    /// The row's domain does not name the company, so it could be lifted by scoped elevation.
    NoCompanyMention,
    /// The row's name differs (report only: the restriction itself is intact).
    Renamed,
    /// A generated row of a model whose declaration no longer generates one (report only: an
    /// extra restriction only narrows).
    Orphan,
}

impl GeneratedIssue {
    /// The problem as a predicate of the sentence "The generated company restriction of M (...)
    /// {problem}" (design §6.4).
    pub fn describe(self) -> String {
        match self {
            Self::Missing => "is missing".to_owned(),
            Self::Archived => "is archived".to_owned(),
            Self::Changed { what } => format!("was changed ({what})"),
            Self::Duplicate { count } => format!("exists {count} times"),
            Self::NoCompanyMention => "does not name the company".to_owned(),
            Self::Renamed => "was renamed".to_owned(),
            Self::Orphan => "belongs to a model that no longer generates one".to_owned(),
        }
    }
}

/// One problem found by [`verify`]: the model it denies (`None` for an orphan row, which belongs to
/// a model the registry does not restrict) and the row it was found on (`None` for a missing row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFinding {
    model: Option<ModelId>,
    model_name: Box<str>,
    row: Option<RuleId>,
    issue: GeneratedIssue,
}

impl GeneratedFinding {
    /// The model the finding denies; `None` for an orphan row.
    pub fn model(&self) -> Option<ModelId> {
        self.model
    }

    /// The model name (also for an orphan).
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// The row the finding was made on; `None` for a missing row.
    pub fn row(&self) -> Option<RuleId> {
        self.row
    }

    /// The problem.
    pub fn issue(&self) -> GeneratedIssue {
        self.issue
    }

    /// Whether the model is denied to every non-`Full` principal until the row is restored: all
    /// problems but a rename and an orphan, and only for a model the registry restricts (a
    /// finding without a model has nothing to deny; it is still an error to report).
    pub fn denies(&self) -> bool {
        self.model.is_some()
            && !matches!(self.issue, GeneratedIssue::Renamed | GeneratedIssue::Orphan)
    }
}

/// Compare the generated rows `rows` (active and archived) with what the registry's declarations
/// generate, and list every problem: sorted by model name, then row id, then problem. Deterministic.
///
/// A row whose model the registry does not know, or whose declaration no longer generates, is an
/// [`GeneratedIssue::Orphan`]. A `generate` failure (a generator bug) is not a finding; call
/// [`generate`] first.
///
/// # Errors
///
/// As [`generate`].
pub fn verify(registry: &Registry, rows: &[GeneratedRow]) -> Result<Vec<GeneratedFinding>> {
    let expected = generate(registry)?;
    Ok(verify_against(registry, &expected, rows))
}

/// [`verify`] against an already generated set (the snapshot compiler generates once and uses
/// the same set to mark the rules it compiles).
#[must_use]
pub fn verify_against(
    registry: &Registry,
    expected: &[GeneratedRestriction],
    rows: &[GeneratedRow],
) -> Vec<GeneratedFinding> {
    let mut by_model: BTreeMap<&str, Vec<&GeneratedRow>> = BTreeMap::new();
    for row in rows {
        by_model.entry(row.model()).or_default().push(row);
    }
    for list in by_model.values_mut() {
        list.sort_by_key(|r| r.id());
    }
    let mut out = Vec::new();
    for want in expected {
        let found = by_model.remove(want.model_name).unwrap_or_default();
        let finding = |row: Option<RuleId>, issue| GeneratedFinding {
            model: Some(want.model),
            model_name: want.model_name.into(),
            row,
            issue,
        };
        if found.is_empty() {
            out.push(finding(None, GeneratedIssue::Missing));
            continue;
        }
        if found.len() > 1 {
            out.push(finding(
                Some(found[0].id()),
                GeneratedIssue::Duplicate { count: found.len() },
            ));
        }
        for row in found {
            let id = Some(row.id());
            if !row.active() {
                out.push(finding(id, GeneratedIssue::Archived));
            }
            if row.operation() != GENERATED_OPERATION {
                out.push(finding(
                    id,
                    GeneratedIssue::Changed {
                        what: Changed::Operation,
                    },
                ));
            }
            if row.group().is_some() {
                out.push(finding(
                    id,
                    GeneratedIssue::Changed {
                        what: Changed::Group,
                    },
                ));
            }
            if row.domain() != Some(want.text.as_str()) {
                out.push(finding(
                    id,
                    GeneratedIssue::Changed {
                        what: Changed::Domain,
                    },
                ));
                if !mentions_company(registry, want.model_name, row.domain()) {
                    out.push(finding(id, GeneratedIssue::NoCompanyMention));
                }
            }
            if row.name() != want.name {
                out.push(finding(id, GeneratedIssue::Renamed));
            }
        }
    }
    for (model, list) in by_model {
        // the registry does not restrict this model: the finding denies nothing
        for row in list {
            let finding = |issue| GeneratedFinding {
                model: None,
                model_name: model.into(),
                row: Some(row.id()),
                issue,
            };
            out.push(finding(GeneratedIssue::Orphan));
            // an orphan with a group is a permission, not a narrowing restriction: say so
            if row.group().is_some() {
                out.push(finding(GeneratedIssue::Changed {
                    what: Changed::Group,
                }));
            }
        }
    }
    out.sort_by(|a, b| (&a.model_name, a.row, a.issue).cmp(&(&b.model_name, b.row, b.issue)));
    out
}

/// Whether `text`, compiled on `model`, names the company. A text that does not compile or a model
/// the registry does not know does not (the row is then also a `Changed` finding).
fn mentions_company(registry: &Registry, model: &str, text: Option<&str>) -> bool {
    registry
        .model(model)
        .and_then(|spec| rule::compile(registry, spec, text).ok())
        .is_some_and(|c| c.mentions_company)
}

/// Compile `text` as a rule on `model` and return the domain and whether it names the company.
/// For the integration fixtures that compare generated and hand-written rows; not a stable API.
///
/// # Errors
///
/// `Programming` for an unknown model; the rule error otherwise.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn compile_for_tests(
    registry: &Registry,
    model: &str,
    text: &str,
) -> Result<(erp_domain::Domain, bool)> {
    let spec = registry
        .model(model)
        .ok_or_else(|| Error::programming(format!("unknown model '{model}'")))?;
    let compiled = rule::compile(registry, spec, Some(text))
        .map_err(|e| Error::programming(format!("{e}")))?;
    Ok((compiled.domain, compiled.mentions_company))
}
