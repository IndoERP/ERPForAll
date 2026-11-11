//! Diagnostics: every problem the compiler finds is a value returned to the caller (standards §1),
//! with a specific kind, the template and the source position.

use crate::limits::{LimitHit, LimitKind};
use crate::names::TemplateName;
use crate::source::Origin;
use std::fmt;
use std::num::NonZeroU32;

/// How serious a diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Severity {
    /// The template (and with [`crate::compile`], the set) does not compile.
    Error,
    /// The template compiles; the author should change something (`t-esc`).
    Warning,
    /// Information for the report (a part of the pipeline a later task adds was skipped).
    Info,
}

impl Severity {
    /// Lower-case name used in reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

/// Why a diagnostic was raised. One variant per reason, so a report can group by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// The XML is not well formed, uses a DTD, a namespace or an entity we refuse.
    Xml,
    /// The file is not `<templates>` with template elements under it.
    FileShape,
    /// A `t-*` attribute that is not a directive.
    UnknownDirective,
    /// A directive on an element where it cannot be used.
    DirectiveNotAllowedHere,
    /// A directive whose value is not what it expects.
    InvalidDirectiveValue,
    /// A directive needs another one that is missing (`t-value` without `t-set`).
    MissingCompanion,
    /// A directive that is rejected on purpose (`t-raw`, `t-call-block`, `t-valuef`).
    RejectedDirective,
    /// `t-foreach` without `t-key`.
    MissingTKey,
    /// `t-elif`/`t-else` not preceded by `t-if`/`t-elif`.
    BranchWithoutIf,
    /// Non-whitespace text between branches.
    TextBetweenBranches,
    /// More than one of `t-if`, `t-elif`, `t-else` on a node.
    MultipleBranchDirectives,
    /// `t-if` and `t-foreach` on the node a `t-elif`/`t-else` follows.
    IfWithForeachChained,
    /// `t-on` or `t-on-` without an event name.
    MissingEventName,
    /// An event modifier outside the supported set.
    UnknownEventModifier,
    /// Event modifiers that cannot work together: `.prevent` with `.passive` (a passive listener
    /// cannot prevent the default action, so the pair would silently do nothing; BUG-070).
    ConflictingEventModifiers,
    /// A component prop suffix outside `.bind`, `.translate`, `.alike`.
    InvalidPropSuffix,
    /// A directive that components do not accept.
    ComponentDirective,
    /// `t-ref` on a component.
    RefOnComponent,
    /// `t-model` on an element other than `input`, `textarea`, `select`.
    ModelOnWrongElement,
    /// `t-custom-<name>` with a name no addon registered.
    CustomDirectiveNotDefined,
    /// A name (attribute, event, slot, local, component) outside its grammar.
    InvalidName,
    /// An expression that does not parse.
    ExpressionSyntax,
    /// Expression syntax the grammar walk refuses.
    ForbiddenSyntax,
    /// An identifier that is neither a local, an allowlisted global nor `this`.
    UnknownIdentifier,
    /// An assignment or update evaluated while rendering.
    WriteDuringRender,
    /// `t-call` or `t-inherit` naming a template that is not in the set.
    MissingTemplate,
    /// Two templates with the same `t-name`.
    DuplicateTemplate,
    /// A static call cycle without an intervening `t-if` guard.
    Cycle,
    /// The inheritance engine refused a combination (wraps its error).
    Inheritance,
    /// A limit was exceeded ([`Diagnostic::limit`] says which, its value and the value found).
    Limit,
    /// A feature the front end does not support yet; the message names the task that adds it.
    NotSupportedUntil,
    /// A directive the code generator does not support yet; the message names the task.
    CodegenNotSupportedUntil,
    /// Markup the HTML parser would restructure, so cloning would not give this tree.
    HtmlStructure,
    /// A deprecated form (`t-esc`).
    Deprecated,
    /// A plain attribute on a `<t>` node: OWL drops it, so it renders nothing (warning).
    IgnoredAttribute,
    /// A pipeline part skipped in this version (the translation pass before UI3-DONE).
    Skipped,
    /// A failure of the compiler itself (a check of its own output failed, an invariant broke).
    /// Never the template author's fault; report it.
    Internal,
}

impl DiagnosticKind {
    /// Stable name used in reports and the survey file.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            DiagnosticKind::Xml => "xml",
            DiagnosticKind::FileShape => "file_shape",
            DiagnosticKind::UnknownDirective => "unknown_directive",
            DiagnosticKind::DirectiveNotAllowedHere => "directive_not_allowed_here",
            DiagnosticKind::InvalidDirectiveValue => "invalid_directive_value",
            DiagnosticKind::MissingCompanion => "missing_companion",
            DiagnosticKind::RejectedDirective => "rejected_directive",
            DiagnosticKind::MissingTKey => "missing_t_key",
            DiagnosticKind::BranchWithoutIf => "branch_without_if",
            DiagnosticKind::TextBetweenBranches => "text_between_branches",
            DiagnosticKind::MultipleBranchDirectives => "multiple_branch_directives",
            DiagnosticKind::IfWithForeachChained => "if_with_foreach_chained",
            DiagnosticKind::MissingEventName => "missing_event_name",
            DiagnosticKind::UnknownEventModifier => "unknown_event_modifier",
            DiagnosticKind::ConflictingEventModifiers => "conflicting_event_modifiers",
            DiagnosticKind::InvalidPropSuffix => "invalid_prop_suffix",
            DiagnosticKind::ComponentDirective => "component_directive",
            DiagnosticKind::RefOnComponent => "ref_on_component",
            DiagnosticKind::ModelOnWrongElement => "model_on_wrong_element",
            DiagnosticKind::CustomDirectiveNotDefined => "custom_directive_not_defined",
            DiagnosticKind::InvalidName => "invalid_name",
            DiagnosticKind::ExpressionSyntax => "expression_syntax",
            DiagnosticKind::ForbiddenSyntax => "forbidden_syntax",
            DiagnosticKind::UnknownIdentifier => "unknown_identifier",
            DiagnosticKind::WriteDuringRender => "write_during_render",
            DiagnosticKind::MissingTemplate => "missing_template",
            DiagnosticKind::DuplicateTemplate => "duplicate_template",
            DiagnosticKind::Cycle => "cycle",
            DiagnosticKind::Inheritance => "inheritance",
            DiagnosticKind::Limit => "limit",
            DiagnosticKind::NotSupportedUntil => "not_supported_until",
            DiagnosticKind::CodegenNotSupportedUntil => "codegen_not_supported_until",
            DiagnosticKind::HtmlStructure => "html_structure",
            DiagnosticKind::Deprecated => "deprecated",
            DiagnosticKind::IgnoredAttribute => "ignored_attribute",
            DiagnosticKind::Skipped => "skipped",
            DiagnosticKind::Internal => "internal",
        }
    }
}

/// A 1-based line and column (in characters).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    line: NonZeroU32,
    column: NonZeroU32,
}

impl Position {
    /// A position; `None` when either value is zero.
    #[must_use]
    pub fn new(line: u32, column: u32) -> Option<Self> {
        Some(Self {
            line: NonZeroU32::new(line)?,
            column: NonZeroU32::new(column)?,
        })
    }

    /// The line.
    #[must_use]
    pub fn line(&self) -> NonZeroU32 {
        self.line
    }

    /// The column.
    #[must_use]
    pub fn column(&self) -> NonZeroU32 {
        self.column
    }
}

/// Where a diagnostic points: the source, and the position in it when known. A combined
/// (inherited) template and a set-level problem have no position; their message names the parts
/// involved.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    origin: Origin,
    position: Option<Position>,
}

impl Location {
    pub(crate) fn new(origin: Origin, position: Option<Position>) -> Self {
        Self { origin, position }
    }

    /// The source.
    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// The position, when known.
    #[must_use]
    pub fn position(&self) -> Option<Position> {
        self.position
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.origin)?;
        if let Some(p) = self.position {
            write!(f, ":{}:{}", p.line, p.column)?;
        }
        Ok(())
    }
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    severity: Severity,
    kind: DiagnosticKind,
    directive: Option<Box<str>>,
    template: Option<TemplateName>,
    location: Option<Box<Location>>,
    limit: Option<LimitHit>,
    message: Box<str>,
    /// Offset in the analysed XML, used internally to resolve combined-source diagnostics.
    source_offset: Option<u32>,
}

impl Diagnostic {
    pub(crate) fn new(
        severity: Severity,
        kind: DiagnosticKind,
        message: impl Into<Box<str>>,
    ) -> Self {
        Self {
            severity,
            kind,
            directive: None,
            template: None,
            location: None,
            limit: None,
            message: message.into(),
            source_offset: None,
        }
    }

    pub(crate) fn error(kind: DiagnosticKind, message: impl Into<Box<str>>) -> Self {
        Self::new(Severity::Error, kind, message)
    }

    /// The error for a limit hit: kind [`DiagnosticKind::Limit`], the standard message, and the
    /// typed data.
    pub(crate) fn limit_hit(hit: LimitHit) -> Self {
        let mut d = Self::error(DiagnosticKind::Limit, hit.message());
        d.limit = Some(hit);
        d
    }

    /// [`Self::limit_hit`] from its parts.
    pub(crate) fn over(kind: LimitKind, limit: usize, actual: usize) -> Self {
        Self::limit_hit(LimitHit::new(kind, limit, actual))
    }

    /// The error for a failure of the compiler itself.
    pub(crate) fn internal(message: impl fmt::Display) -> Self {
        Self::error(
            DiagnosticKind::Internal,
            format!("internal compiler error: {message}"),
        )
    }

    /// Replaces the message (a limit diagnostic whose standard text needs a note).
    pub(crate) fn with_message(mut self, message: impl Into<Box<str>>) -> Self {
        self.message = message.into();
        self
    }

    pub(crate) fn with_source_offset(mut self, offset: u32) -> Self {
        self.source_offset = Some(offset);
        self
    }

    pub(crate) fn source_offset(&self) -> Option<u32> {
        self.source_offset
    }

    pub(crate) fn without_location(mut self) -> Self {
        self.location = None;
        self
    }

    /// Appends a note to the message.
    pub(crate) fn with_note(mut self, note: &str) -> Self {
        let mut m = String::from(self.message);
        m.push_str(note);
        self.message = m.into();
        self
    }

    pub(crate) fn with_directive(mut self, directive: &str) -> Self {
        self.directive = Some(directive.into());
        self
    }

    pub(crate) fn with_template(mut self, template: Option<&TemplateName>) -> Self {
        self.template = template.cloned();
        self
    }

    pub(crate) fn with_location(mut self, location: Location) -> Self {
        self.location = Some(Box::new(location));
        self
    }

    /// Severity.
    #[must_use]
    pub fn severity(&self) -> Severity {
        self.severity
    }

    /// Kind.
    #[must_use]
    pub fn kind(&self) -> DiagnosticKind {
        self.kind
    }

    /// What it is about, if anything: a QWeb directive name (`t-foreach`, `t-on`, `t-att`, …; an
    /// unknown directive as written), or one of `expression`, `attribute`, `element`, `prop`,
    /// `component`, `translation`.
    #[must_use]
    pub fn directive(&self) -> Option<&str> {
        self.directive.as_deref()
    }

    /// The template it is about, if any.
    #[must_use]
    pub fn template(&self) -> Option<&TemplateName> {
        self.template.as_ref()
    }

    /// The source position, if any.
    #[must_use]
    pub fn location(&self) -> Option<&Location> {
        self.location.as_deref()
    }

    /// The limit that was hit, for a [`DiagnosticKind::Limit`] diagnostic (always set for one).
    #[must_use]
    pub fn limit(&self) -> Option<&LimitHit> {
        self.limit.as_ref()
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}[{}]", self.severity.name(), self.kind.code())?;
        if let Some(location) = &self.location {
            write!(f, " {location}")?;
        }
        if let Some(template) = &self.template {
            write!(f, " template {template}")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// The errors of a [`crate::compile`] call: every error diagnostic (never only the first), and the
/// warnings found on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileErrors {
    errors: Vec<Diagnostic>,
    warnings: Vec<Diagnostic>,
}

impl CompileErrors {
    pub(crate) fn new(errors: Vec<Diagnostic>, warnings: Vec<Diagnostic>) -> Self {
        Self { errors, warnings }
    }

    /// The errors, grouped by phase: set-level front-end errors, then each template's front-end
    /// errors in set order, then each passed template's code-generation errors in set order, then
    /// the set's output limit.
    #[must_use]
    pub fn errors(&self) -> &[Diagnostic] {
        &self.errors
    }

    /// Every diagnostic that is not an error: warnings and information notes (in the same order
    /// as [`Self::errors`]).
    #[must_use]
    pub fn warnings(&self) -> &[Diagnostic] {
        &self.warnings
    }
}

impl fmt::Display for CompileErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} template error(s)", self.errors.len())?;
        for error in &self.errors {
            write!(f, "\n  {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CompileErrors {}
