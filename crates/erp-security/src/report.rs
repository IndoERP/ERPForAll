//! The build report of a policy snapshot (DN §2.3, §3.1): what did not compile and what looks
//! wrong, returned with the snapshot (never only logged). logs it once per
//! generation, exports `access_rule_errors{model}` and prints it with `erp access report`.

use std::fmt;
use std::time::Duration;

use erp_orm::{GroupId, OperationSet, RuleRef};

use crate::company_access::GeneratedFinding;
use crate::group_spec::GroupSpecError;
use crate::rows::operation_string;
use crate::rule::RuleError;

/// A rule that does not compile: it denies its model's operations for everyone it applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleIssue {
    rule: RuleRef,
    model: String,
    ops: OperationSet,
    error: RuleError,
}

impl RuleIssue {
    pub(crate) fn new(rule: RuleRef, model: String, ops: OperationSet, error: RuleError) -> Self {
        Self {
            rule,
            model,
            ops,
            error,
        }
    }

    /// The rule.
    pub fn rule(&self) -> &RuleRef {
        &self.rule
    }

    /// The rule's model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The operations it applies to.
    pub fn ops(&self) -> OperationSet {
        self.ops
    }

    /// Why it does not compile.
    pub fn error(&self) -> &RuleError {
        &self.error
    }
}

impl fmt::Display for RuleIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Access rule '{}' ({}) on {} cannot be compiled: {} [denies {}]",
            self.rule.name(),
            self.rule.id(),
            self.model,
            self.error,
            operation_string(self.ops)
        )
    }
}

/// A problem with a model's generated company restriction (design `m3-company-a1` §6.4): the
/// row is missing, archived, changed, duplicated, renamed or an orphan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedIssueReport {
    finding: GeneratedFinding,
    text: String,
}

impl GeneratedIssueReport {
    pub(crate) fn new(finding: GeneratedFinding, text: String) -> Self {
        Self { finding, text }
    }

    /// The finding.
    pub fn finding(&self) -> &GeneratedFinding {
        &self.finding
    }

    /// The model name.
    pub fn model(&self) -> &str {
        self.finding.model_name()
    }

    /// Whether the model is refused to every non-`Full` principal because of it.
    pub fn denies(&self) -> bool {
        self.finding.denies()
    }
}

impl fmt::Display for GeneratedIssueReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.text)?;
        if self.finding.denies() {
            write!(f, " [denies {}]", self.finding.model_name())?;
        }
        Ok(())
    }
}

/// Where a field's group rule comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GroupsSource {
    /// The `groups` of the field's declaration (the registry).
    Registry,
    /// The group list of a custom field (`ir.model.fields`).
    Overlay,
}

impl fmt::Display for GroupsSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Registry => "declared groups",
            Self::Overlay => "custom field groups",
        })
    }
}

/// What is wrong with a field's group rule.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FieldGroupsProblem {
    /// The spec does not compile: the field is denied to everyone outside superuser mode.
    BadSpec(GroupSpecError),
    /// A custom-field row names a model the registry does not have (it restricts nothing; an
    /// error of the build).
    UnknownModel,
    /// A custom-field row names a field the model does not have (it restricts nothing; an error
    /// of the build).
    UnknownField,
}

/// A field whose group rule is broken or dangling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldGroupsIssue {
    model: String,
    field: String,
    source: GroupsSource,
    problem: FieldGroupsProblem,
}

impl FieldGroupsIssue {
    pub(crate) fn new(
        model: &str,
        field: &str,
        source: GroupsSource,
        problem: FieldGroupsProblem,
    ) -> Self {
        Self {
            model: model.to_owned(),
            field: field.to_owned(),
            source,
            problem,
        }
    }

    /// The model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The field name.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// Where the rule comes from.
    pub fn source(&self) -> GroupsSource {
        self.source
    }

    /// What is wrong.
    pub fn problem(&self) -> &FieldGroupsProblem {
        &self.problem
    }

    /// Whether the problem denies its field (a spec that does not compile) rather than naming an
    /// unknown model or field, which restricts nothing and is dropped. Every issue is an error of
    /// the build: a typo in an overlay row would leave the field readable by everyone it was meant
    /// to hide it from, so the row must be fixed or removed.
    pub fn denies(&self) -> bool {
        matches!(self.problem, FieldGroupsProblem::BadSpec(_))
    }
}

impl fmt::Display for FieldGroupsIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.problem {
            FieldGroupsProblem::BadSpec(error) => write!(
                f,
                "field '{}' on {} ({}): {error} [denies the field]",
                self.field, self.model, self.source
            ),
            FieldGroupsProblem::UnknownModel => write!(
                f,
                "{} for '{}' on {}: the model does not exist",
                self.source, self.field, self.model
            ),
            FieldGroupsProblem::UnknownField => write!(
                f,
                "{} for '{}' on {}: the model has no such field",
                self.source, self.field, self.model
            ),
        }
    }
}

/// An `ir_capability` row whose group spec does not compile: asking for the capability fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityIssue {
    name: String,
    error: GroupSpecError,
}

impl CapabilityIssue {
    pub(crate) fn new(name: &str, error: GroupSpecError) -> Self {
        Self {
            name: name.to_owned(),
            error,
        }
    }

    /// The capability name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Why the spec does not compile.
    pub fn error(&self) -> &GroupSpecError {
        &self.error
    }
}

impl fmt::Display for CapabilityIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "capability '{}': {} [asking for it fails]",
            self.name, self.error
        )
    }
}

/// Counts of a build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct BuildCounts {
    /// Rules read.
    pub rules: usize,
    /// Rules that compiled.
    pub compiled: usize,
    /// Groups.
    pub groups: usize,
    /// Implications.
    pub implications: usize,
    /// Models with at least one rule.
    pub models: usize,
}

/// What a snapshot build found besides the snapshot itself.
#[derive(Debug, Clone, Default)]
pub struct BuildReport {
    pub(crate) rule_errors: Vec<RuleIssue>,
    pub(crate) generated_issues: Vec<GeneratedIssueReport>,
    pub(crate) group_cycles: Vec<Vec<GroupId>>,
    pub(crate) missing_capabilities: Vec<&'static str>,
    pub(crate) unknown_capabilities: Vec<String>,
    pub(crate) capability_errors: Vec<CapabilityIssue>,
    pub(crate) field_group_issues: Vec<FieldGroupsIssue>,
    pub(crate) counts: BuildCounts,
    pub(crate) duration: Duration,
}

impl BuildReport {
    /// Rules that do not compile (each denies its model's operations), by rule id. Rules whose
    /// model is not in the registry are listed too (they deny nothing: there is no model).
    pub fn rule_errors(&self) -> &[RuleIssue] {
        &self.rule_errors
    }

    /// Problems with generated company restrictions (design `m3-company-a1` §6.4), by model and
    /// row. All are errors; those with [`GeneratedIssueReport::denies`] refuse their model.
    pub fn generated_issues(&self) -> &[GeneratedIssueReport] {
        &self.generated_issues
    }

    /// Rule errors per model name, sorted by model (the `access_rule_errors{model}` metric);
    /// generated-restriction problems count with the model they are on.
    pub fn errors_by_model(&self) -> Vec<(&str, usize)> {
        let mut out: Vec<(&str, usize)> = Vec::new();
        let mut models: Vec<&str> = self
            .rule_errors
            .iter()
            .map(|e| e.model.as_str())
            .chain(
                self.generated_issues
                    .iter()
                    .map(GeneratedIssueReport::model),
            )
            .collect();
        models.sort_unstable();
        for model in models {
            match out.last_mut() {
                Some((last, count)) if *last == model => *count += 1,
                Some(_) | None => out.push((model, 1)),
            }
        }
        out
    }

    pub fn group_cycles(&self) -> &[Vec<GroupId>] {
        &self.group_cycles
    }

    /// Registered capabilities (`erp_orm::capability!`) without an `ir_capability` row: asking
    /// for them is an error.
    pub fn missing_capabilities(&self) -> &[&'static str] {
        &self.missing_capabilities
    }

    /// `ir_capability` rows naming no registered capability (an uninstalled addon's row, a
    /// typo): they grant nothing.
    pub fn unknown_capabilities(&self) -> &[String] {
        &self.unknown_capabilities
    }

    /// `ir_capability` rows whose group spec does not compile (an unknown group): asking for the
    /// capability fails, by name.
    pub fn capability_errors(&self) -> &[CapabilityIssue] {
        &self.capability_errors
    }

    /// Field group rules that do not compile (the field is denied outside superuser mode) or that
    /// name a model or field the registry does not have (they restrict nothing), by model and
    /// field.
    pub fn field_group_issues(&self) -> &[FieldGroupsIssue] {
        &self.field_group_issues
    }

    /// Counts.
    pub fn counts(&self) -> BuildCounts {
        self.counts
    }

    /// How long the build took (compile only; the loader's statements are not counted).
    pub fn duration(&self) -> Duration {
        self.duration
    }

    /// Whether the build found an error (a rule error, a missing or broken capability mapping,
    /// a field group rule that denies its field).
    pub fn has_errors(&self) -> bool {
        !self.rule_errors.is_empty()
            || !self.generated_issues.is_empty()
            || !self.missing_capabilities.is_empty()
            || !self.capability_errors.is_empty()
            || !self.field_group_issues.is_empty()
    }
}

impl fmt::Display for BuildReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.counts;
        writeln!(
            f,
            "{} rules ({} compiled) on {} models, {} groups, {} implications",
            c.rules, c.compiled, c.models, c.groups, c.implications
        )?;
        for issue in &self.rule_errors {
            writeln!(f, "error: {issue}")?;
        }
        for issue in &self.generated_issues {
            writeln!(f, "error: {issue}")?;
        }
        for name in &self.missing_capabilities {
            writeln!(
                f,
                "error: capability '{name}' is registered but has no ir_capability row: asking \
                 for it fails"
            )?;
        }
        for issue in &self.capability_errors {
            writeln!(f, "error: {issue}")?;
        }
        for issue in &self.field_group_issues {
            writeln!(f, "error: {issue}")?;
        }
        for name in &self.unknown_capabilities {
            writeln!(
                f,
                "warning: ir_capability row '{name}' names no registered capability"
            )?;
        }
        for cycle in &self.group_cycles {
            let ids: Vec<String> = cycle.iter().map(ToString::to_string).collect();
            writeln!(
                f,
                "note: groups {} imply each other (a cycle)",
                ids.join(", ")
            )?;
        }
        Ok(())
    }
}
