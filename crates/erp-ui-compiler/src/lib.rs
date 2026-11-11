//! `erp-ui-compiler`: the Elang template compiler (design note
//! ). A pure crate: no I/O, threads or clock, the same bytes from
//! the same input on every host, native and WASM.
//!
//! ```
//! use erp_ui_compiler::{
//!     AddonName, BuildMode, CompileOptions, TemplatePath, TemplateSet, TemplateSource, analyze,
//!     compile,
//! };
//!
//! let mut set = TemplateSet::new();
//! set.push(TemplateSource::addon_file(
//!     AddonName::new("web")?,
//!     TemplatePath::new("static/src/hello.xml")?,
//!     r#"<templates><t t-name="web.Hello"><p class="o_hello" t-out="this.props.name"/></t></templates>"#,
//! ));
//! let options = CompileOptions::new(BuildMode::Prod);
//! let analysis = analyze(&set, &options);
//! assert_eq!(analysis.passed(), 1);
//! let compiled = compile(&set, &options).map_err(|e| e.to_string())?;
//! let module = &compiled.modules()[0];
//! assert_eq!(module.name().as_str(), "web.Hello");
//! assert!(module.code().contains("$.insert($n0, () => $self.props.name, null);"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Every problem is a [`Diagnostic`]; [`analyze`] never fails, [`compile`] fails with every error
//! of the set ([`CompileErrors`]).

mod analyze;
mod codegen;
mod diag;
mod expr;
#[cfg(feature = "html-budget")]
pub mod html_budget;
mod inherit;
mod ir;
mod jsstr;
mod limits;
mod names;
mod prelex;
mod provenance;
mod source;
mod sourcemap;
mod translation;
mod xml;

pub use codegen::ABI_MODULE;
pub use diag::{CompileErrors, Diagnostic, DiagnosticKind, Location, Position, Severity};
pub use limits::{CompilerLimits, LimitError, LimitHit, LimitKind};
pub use names::{AddonName, DirectiveName, NameError, TemplateName, TemplatePath};
pub use source::{
    BuildMode, CompileOptions, MAX_CUSTOM_DIRECTIVES, Origin, SourceKind, TemplateSet,
    TemplateSource,
};
pub use sourcemap::{ProvenanceEntry, SourceMap};
pub use translation::{
    ExtractedTranslations, MAX_TRANSLATION_BYTES, MAX_TRANSLATION_TERM_BYTES,
    MAX_TRANSLATION_TERMS, TranslationCatalogue, TranslationError, TranslationTerm,
};

use analyze::{LocCtx, T_MARKERS, TemplateAnalysis, UI3_DONE, analyze_template};
use expr::ExprParser;
use inherit::{Entry, EntryId, Role};
use ir::Pos;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use xml::{LineIndex, TopLevel};

// Every entry of a set gets a `u32` id: the `templates` limit keeps the count below it.
const _: () = assert!(LimitKind::Templates.default_value() <= EntryId::MAX as usize);
// Our limits are passed to the inheritance engine, which accepts only values up to its own.
const _: () = {
    let engine = erp_views::ViewLimits::DEFAULT;
    assert!(LimitKind::Depth.default_value() <= engine.get(erp_views::LimitKind::Depth));
    assert!(
        LimitKind::FileBytes.default_value() <= engine.get(erp_views::LimitKind::CombinedBytes)
    );
    assert!(
        LimitKind::FileNodes.default_value() <= engine.get(erp_views::LimitKind::CombinedNodes)
    );
};

/// Whether a template passed the front end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verdict {
    /// No error diagnostic.
    Passed,
    /// At least one error diagnostic.
    Rejected,
}

/// One template of an [`Analysis`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateReport {
    name: TemplateName,
    location: Location,
    verdict: Verdict,
    diagnostics: Vec<Diagnostic>,
}

impl TemplateReport {
    /// The template name.
    #[must_use]
    pub fn name(&self) -> &TemplateName {
        &self.name
    }

    /// Where the template is declared.
    #[must_use]
    pub fn location(&self) -> &Location {
        &self.location
    }

    /// The verdict.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        self.verdict
    }

    /// Its diagnostics, in document order.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}

/// The front end's report on a template set: every template's verdict and diagnostics, and the
/// diagnostics that belong to no template (an unreadable file, an extension of a missing
/// template, a set over its limit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    files: usize,
    extensions: usize,
    templates: Vec<TemplateReport>,
    set_diagnostics: Vec<Diagnostic>,
}

impl Analysis {
    /// Number of files read.
    #[must_use]
    pub fn files(&self) -> usize {
        self.files
    }

    /// Number of extensions (`t-inherit-mode="extension"`) read.
    #[must_use]
    pub fn extensions(&self) -> usize {
        self.extensions
    }

    /// The templates, in set order.
    #[must_use]
    pub fn templates(&self) -> &[TemplateReport] {
        &self.templates
    }

    /// Diagnostics that belong to no template.
    #[must_use]
    pub fn set_diagnostics(&self) -> &[Diagnostic] {
        &self.set_diagnostics
    }

    /// Templates that passed.
    #[must_use]
    pub fn passed(&self) -> usize {
        self.templates
            .iter()
            .filter(|t| t.verdict == Verdict::Passed)
            .count()
    }

    /// Templates that were rejected.
    #[must_use]
    pub fn rejected(&self) -> usize {
        self.templates.len() - self.passed()
    }

    /// Whether the set has no error at all.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.rejected() == 0
            && !self
                .set_diagnostics
                .iter()
                .any(|d| d.severity() == Severity::Error)
    }

    /// The canonical byte form (design note §4): one line per template and per diagnostic, in
    /// set order. Native and WASM builds produce the same bytes.
    pub fn write_canonical(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "erpforall-ui-analysis 1\nfiles {}\nextensions {}\ntemplates {} passed {} rejected {}",
            self.files,
            self.extensions,
            self.templates.len(),
            self.passed(),
            self.rejected()
        );
        for d in &self.set_diagnostics {
            let _ = writeln!(out, "set {}", canonical_diagnostic(d));
        }
        for t in &self.templates {
            let verdict = match t.verdict {
                Verdict::Passed => "passed",
                Verdict::Rejected => "rejected",
            };
            let header = format!("template {} {} {verdict}", t.name, t.location);
            let _ = writeln!(out, "{}", one_line(&header));
            for d in &t.diagnostics {
                let _ = writeln!(out, "  {}", canonical_diagnostic(d));
            }
        }
    }
}

fn canonical_diagnostic(d: &Diagnostic) -> String {
    one_line(&d.to_string())
}

/// One canonical line: line breaks (from source text in messages, or any other part) are
/// escaped, so no input can forge a line of the canonical form (rust review L4).
fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

/// One generated module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledModule {
    name: TemplateName,
    code: String,
    source_map: SourceMap,
}

impl CompiledModule {
    /// The template name.
    #[must_use]
    pub fn name(&self) -> &TemplateName {
        &self.name
    }

    /// The ES module text.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The ES module text, taken (no copy).
    #[must_use]
    pub fn into_code(self) -> String {
        self.code
    }

    /// The source map with provenance tracking.
    #[must_use]
    pub fn source_map(&self) -> &SourceMap {
        &self.source_map
    }
}

/// The modules of a compiled set, in template order, and the warnings found on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledSet {
    modules: Vec<CompiledModule>,
    warnings: Vec<Diagnostic>,
}

impl CompiledSet {
    /// The modules.
    #[must_use]
    pub fn modules(&self) -> &[CompiledModule] {
        &self.modules
    }

    /// The modules, taken (no copy).
    #[must_use]
    pub fn into_modules(self) -> Vec<CompiledModule> {
        self.modules
    }

    /// The module of template `name`, if the set has it.
    #[must_use]
    pub fn module(&self, name: &TemplateName) -> Option<&CompiledModule> {
        self.modules.iter().find(|m| &m.name == name)
    }

    /// Every diagnostic that is not an error: warnings and information notes.
    #[must_use]
    pub fn warnings(&self) -> &[Diagnostic] {
        &self.warnings
    }

    /// The canonical byte form: a header, then each module as `module <name> <bytes>` and its
    /// text, then the warnings.
    pub fn write_canonical(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "erpforall-ui-compiled 1\nmodules {}",
            self.modules.len()
        );
        for m in &self.modules {
            let _ = writeln!(out, "module {} {}", m.name, m.code.len());
            out.push_str(&m.code);
        }
        for w in &self.warnings {
            let _ = writeln!(out, "warning {}", canonical_diagnostic(w));
        }
    }
}

impl CompileErrors {
    /// The canonical byte form of the errors and warnings.
    pub fn write_canonical(&self, out: &mut String) {
        let _ = writeln!(out, "erpforall-ui-errors 1\nerrors {}", self.errors().len());
        for e in self.errors() {
            let _ = writeln!(out, "error {}", canonical_diagnostic(e));
        }
        for w in self.warnings() {
            let _ = writeln!(out, "warning {}", canonical_diagnostic(w));
        }
    }
}

/// Runs the front end over a template set and reports every template's verdict. Never fails:
/// every problem is a diagnostic in the report.
#[must_use]
pub fn analyze(set: &TemplateSet, options: &CompileOptions) -> Analysis {
    run(set, options).analysis
}

/// Compiles a template set: the front end, then code generation for every template the front end
/// passed. Any error fails the whole set; every error is returned, code-generation errors of the
/// passed templates included when other templates failed the front end, so one run reports all.
///
/// # Errors
/// [`CompileErrors`] with every error diagnostic of the set: front-end errors, constructs whose
/// code generation a later task adds (`CodegenNotSupportedUntil`), markup the HTML parser would
/// restructure (`HtmlStructure`) and output limits.
pub fn compile(set: &TemplateSet, options: &CompileOptions) -> Result<CompiledSet, CompileErrors> {
    compile_run(set, options, run(set, options))
}

/// Compile selected static literals using an explicit immutable catalogue.
///
/// This pure API does not install a language or activate localized serving. Assembly must
/// include catalogue identity in its bundle key before integrating this API. Existing
/// [`compile`] and [`CompileOptions`] remain source-mode contracts.
///
/// # Errors
/// [`CompileErrors`] for the usual compiler failures, ambiguous inherited literal ownership,
/// unsupported caller-context propagation, unproven text/attribute mutation through inheritance,
/// or translation admission limits. Untranslated semantic and HTML/codegen admission happens
/// before substitutions; the generated preflight output is discarded before localized output.
///
/// # Examples
///
/// ```
/// use erp_ui_compiler::{
///     AddonName, BuildMode, CompileOptions, TemplatePath, TemplateSet, TemplateSource,
///     TranslationCatalogue, compile_localized,
/// };
///
/// let mut set = TemplateSet::new();
/// set.push(TemplateSource::addon_file(
///     AddonName::new("web")?,
///     TemplatePath::new("static/src/hello.xml")?,
///     r#"<templates><t t-name="web.Hello"><p>Hello</p></t></templates>"#,
/// ));
/// let catalogue = TranslationCatalogue::new("fr_FR")?
///     .with_term(AddonName::new("web")?, "Hello", "Bonjour")?;
/// let compiled = compile_localized(&set, &CompileOptions::new(BuildMode::Prod), &catalogue)?;
/// assert!(compiled.modules()[0].code().contains("Bonjour"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn compile_localized(
    set: &TemplateSet,
    options: &CompileOptions,
    catalogue: &TranslationCatalogue,
) -> Result<CompiledSet, CompileErrors> {
    validate_localized_source(set, options)?;
    let mut pass = translation::Pass::new(Some(catalogue), options.limits());
    let run = run_with_translation(set, options, Some(&mut pass));
    compile_run(set, options, run)
}

/// Extract the static literals selected by localized compilation, with original occurrences.
///
/// Terms are sorted by effective module/source and their locations are sorted/deduplicated.
/// This performs no I/O, code execution or POT encoding; the CLI/catalogue delivery owns those.
///
/// # Errors
/// [`CompileErrors`] for malformed templates, ambiguous ownership, unsupported translation
/// contexts or bounded extraction/storage admission. Source semantic and HTML/codegen
/// admission runs before extraction. Inherited text after text-mutating operations and selected
/// attributes after attribute writes are refused until literal-level provenance exists. Equal
/// or restored attribute values do not erase mutation history. No partial result is returned.
///
/// # Examples
///
/// ```
/// use erp_ui_compiler::{
///     AddonName, BuildMode, CompileOptions, TemplatePath, TemplateSet, TemplateSource,
///     extract_translations,
/// };
///
/// let mut set = TemplateSet::new();
/// set.push(TemplateSource::addon_file(
///     AddonName::new("example")?,
///     TemplatePath::new("static/src/hello.xml")?,
///     r#"<templates><t t-name="example.Hello"><p>Hello</p><b>Hello</b></t></templates>"#,
/// ));
/// let extracted = extract_translations(&set, &CompileOptions::new(BuildMode::Prod))?;
/// assert_eq!(extracted.terms().len(), 1);
/// let term = &extracted.terms()[0];
/// assert_eq!((term.addon().as_str(), term.source()), ("example", "Hello"));
/// assert_eq!(term.locations().len(), 2);
/// assert_eq!(term.locations()[0].origin().path().as_str(), "static/src/hello.xml");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn extract_translations(
    set: &TemplateSet,
    options: &CompileOptions,
) -> Result<ExtractedTranslations, CompileErrors> {
    validate_localized_source(set, options)?;
    let mut pass = translation::Pass::new(None, options.limits());
    let run = run_with_translation(set, options, Some(&mut pass));
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for diagnostic in run.analysis.set_diagnostics.iter().chain(
        run.analysis
            .templates
            .iter()
            .flat_map(|template| template.diagnostics.iter()),
    ) {
        match diagnostic.severity() {
            Severity::Error => errors.push(diagnostic.clone()),
            Severity::Warning | Severity::Info => warnings.push(diagnostic.clone()),
        }
    }
    if errors.is_empty() {
        Ok(pass.finish())
    } else {
        Err(CompileErrors::new(errors, warnings))
    }
}

/// Check original semantic and HTML/codegen admission before a catalogue can erase invalid
/// source literals. This pass consumes supported context syntax but performs no substitutions
/// or text ownership claims. Its generated output is dropped before the actual pass begins.
fn validate_localized_source(
    set: &TemplateSet,
    options: &CompileOptions,
) -> Result<(), CompileErrors> {
    let mut pass = translation::Pass::validation(options.limits());
    let run = run_with_translation(set, options, Some(&mut pass));
    compile_run(set, options, run).map(|_| ())
}

fn compile_run(
    set: &TemplateSet,
    options: &CompileOptions,
    run: Run,
) -> Result<CompiledSet, CompileErrors> {
    let Run {
        analysis,
        analysed,
        lines,
    } = run;
    let mut errors: Vec<Diagnostic> = Vec::new();
    let mut warnings: Vec<Diagnostic> = Vec::new();
    let all = analysis
        .set_diagnostics
        .iter()
        .chain(analysis.templates.iter().flat_map(|t| t.diagnostics.iter()));
    for d in all {
        match d.severity() {
            Severity::Error => errors.push(d.clone()),
            _ => warnings.push(d.clone()),
        }
    }
    let limits = options.limits();
    let max_module = limits.get(LimitKind::ModuleBytes);
    let max_output = limits.get(LimitKind::OutputBytes);
    let mut total = 0usize;
    let mut modules = Vec::with_capacity(analysed.len());
    let mut arena = oxc_allocator::Allocator::new();
    for a in &analysed {
        let template = &a.analysis.ir.name;
        let Some(f) = set.files().get(a.file) else {
            errors.push(
                Diagnostic::internal(format!(
                    "template '{template}' points at file {} of a set of {}",
                    a.file,
                    set.len()
                ))
                .with_template(Some(template)),
            );
            continue;
        };
        let (text, line_index) = match &a.text {
            AnalysedText::File => (f.xml(), lines.get(a.file)),
            AnalysedText::Combined { xml, .. } => (xml.as_str(), None),
        };
        let with_note = |d: Diagnostic| match &a.text {
            AnalysedText::Combined { provenance, .. } => provenance.remap(d),
            AnalysedText::File => d,
        };
        let loc = LocCtx {
            origin: f.origin(),
            text,
            lines: line_index,
        };
        match codegen::generate(&a.analysis.ir, &loc, limits) {
            Err(errs) => errors.extend(errs.into_iter().map(with_note)),
            Ok((code, source_map)) => {
                if code.len() > max_module {
                    errors.push(
                        Diagnostic::over(LimitKind::ModuleBytes, max_module, code.len())
                            .with_template(Some(template))
                            .with_location(a.location.clone()),
                    );
                    continue;
                }
                if let Err(message) = codegen::verify_module(&mut arena, &code, options.mode()) {
                    errors.push(
                        Diagnostic::internal(message)
                            .with_template(Some(template))
                            .with_location(a.location.clone()),
                    );
                    continue;
                }
                total = total.saturating_add(code.len());
                if total > max_output {
                    // Stop here: peak memory stays bounded by the limit (rust review M5).
                    errors.push(Diagnostic::over(LimitKind::OutputBytes, max_output, total));
                    break;
                }
                modules.push(CompiledModule {
                    name: template.clone(),
                    code,
                    source_map,
                });
            }
        }
    }
    if errors.is_empty() {
        Ok(CompiledSet { modules, warnings })
    } else {
        Err(CompileErrors::new(errors, warnings))
    }
}

/// The text an analysed template's offsets point into.
enum AnalysedText {
    /// Its source file (shared by every template of the file, never copied per template).
    File,
    /// The combined template the inheritance engine produced, and the note naming the parts it
    /// was built from (appended to its diagnostics, which carry no position).
    Combined {
        xml: String,
        provenance: provenance::Mapping,
    },
}

/// One analysed template kept for code generation.
struct Analysed {
    /// Index of its file in the set.
    file: usize,
    text: AnalysedText,
    location: Location,
    analysis: TemplateAnalysis,
}

struct Run {
    analysis: Analysis,
    /// Templates that passed, in set order.
    analysed: Vec<Analysed>,
    /// The line index of each file of the set.
    lines: Vec<LineIndex>,
}

/// A top-level child of a file, classified.
struct Top<'s> {
    file: usize,
    top: &'s TopLevel<'s>,
    /// `None` when its name or role could not be read (its problems are recorded).
    role: Option<Role>,
    location: Location,
    /// Original offsets in the engine's breadth-first operation order, computed once.
    spec_offsets: Vec<u32>,
}

impl Run {
    /// A run that stopped before analysing any template.
    fn stopped(files: usize, set_diagnostics: Vec<Diagnostic>) -> Self {
        Run {
            analysis: Analysis {
                files,
                extensions: 0,
                templates: Vec::new(),
                set_diagnostics,
            },
            analysed: Vec::new(),
            lines: Vec::new(),
        }
    }
}

/// The inheritance engine's limits: our depth, and our file limits for the combined output
/// (security review L-e). The values are at most the engine's (const assertions above).
fn view_limits(limits: &CompilerLimits) -> Result<erp_views::ViewLimits, Diagnostic> {
    let lower = |l: erp_views::ViewLimits, kind, value| {
        l.lowered(kind, value).map_err(|e| {
            Diagnostic::internal(format!(
                "the inheritance engine refused the limit {value}: {e}"
            ))
        })
    };
    let l = erp_views::ViewLimits::default();
    let l = lower(l, erp_views::LimitKind::Depth, limits.get(LimitKind::Depth))?;
    let l = lower(
        l,
        erp_views::LimitKind::CombinedBytes,
        limits.get(LimitKind::FileBytes),
    )?;
    lower(
        l,
        erp_views::LimitKind::CombinedNodes,
        limits.get(LimitKind::FileNodes),
    )
}

fn run(set: &TemplateSet, options: &CompileOptions) -> Run {
    run_with_translation(set, options, None)
}

#[allow(
    clippy::too_many_lines,
    reason = "the phases of one run share their tables; each phase is commented"
)]
fn run_with_translation(
    set: &TemplateSet,
    options: &CompileOptions,
    mut translation: Option<&mut translation::Pass<'_>>,
) -> Run {
    let limits = options.limits();
    let mut set_diags: Vec<Diagnostic> = Vec::new();
    let files = set.files();

    // 0. Set-level input bounds, before anything is parsed (security review S5).
    let max_files = limits.get(LimitKind::Files);
    if files.len() > max_files {
        set_diags.push(Diagnostic::over(LimitKind::Files, max_files, files.len()));
        return Run::stopped(files.len(), set_diags);
    }
    let input_bytes = files
        .iter()
        .fold(0usize, |acc, f| acc.saturating_add(f.xml().len()));
    let max_input = limits.get(LimitKind::InputBytes);
    if input_bytes > max_input {
        set_diags.push(Diagnostic::over(
            LimitKind::InputBytes,
            max_input,
            input_bytes,
        ));
        return Run::stopped(files.len(), set_diags);
    }
    let view_limits = match view_limits(limits) {
        Ok(l) => l,
        Err(d) => {
            set_diags.push(d);
            return Run::stopped(files.len(), set_diags);
        }
    };

    // 1. Parse every file; the template count is checked per file before its trees are built.
    let mut parsed: Vec<(Vec<TopLevel<'_>>, LineIndex)> = Vec::with_capacity(files.len());
    let mut templates_so_far = 0usize;
    for f in files {
        let lines = LineIndex::new(f.xml());
        match xml::parse_templates_file(f.xml(), limits, templates_so_far) {
            Ok(tops) => {
                templates_so_far += tops.len();
                parsed.push((tops, lines));
            }
            Err(problem) => {
                let position = match (problem.offset, problem.line_column) {
                    (Some(off), _) => lines.position(f.xml(), off),
                    (None, Some((line, column))) => Position::new(line, column),
                    (None, None) => None,
                };
                let d = match problem.limit {
                    Some(hit) => Diagnostic::limit_hit(hit).with_message(problem.message),
                    None => Diagnostic::error(problem.kind, problem.message),
                }
                .with_location(Location::new(f.origin().clone(), position));
                let set_over = problem
                    .limit
                    .is_some_and(|hit| hit.kind() == LimitKind::Templates);
                set_diags.push(d);
                if set_over {
                    // A set-level limit: nothing after this file is read.
                    return Run::stopped(files.len(), set_diags);
                }
                parsed.push((Vec::new(), lines));
            }
        }
    }
    let file_loc = |file: usize, offset: usize| -> Location {
        let f = &files[file];
        Location::new(f.origin().clone(), parsed[file].1.position(f.xml(), offset))
    };

    // 2. Classify the top-level children.
    let mut tops: Vec<Top<'_>> = Vec::with_capacity(templates_so_far);
    let mut extra: BTreeMap<usize, Vec<Diagnostic>> = BTreeMap::new();
    for (fi, (file_tops, _)) in parsed.iter().enumerate() {
        for top in file_tops {
            let el = &top.element;
            let location = file_loc(fi, top.range.start);
            let at_attr = |name: &str| {
                el.attr_node(name)
                    .map_or(top.range.start, |a| a.offset as usize)
            };
            let name_attr = el.attr("t-name");
            let inherit = el.attr("t-inherit");
            let mode = el.attr("t-inherit-mode");
            let mut problems: Vec<Diagnostic> = Vec::new();
            let mut problem = |kind, directive: &str, offset: usize, message: String| {
                problems.push(
                    Diagnostic::error(kind, message)
                        .with_directive(directive)
                        .with_location(file_loc(fi, offset)),
                );
            };
            let name = match name_attr.map(TemplateName::new) {
                Some(Ok(n)) => Some(n),
                Some(Err(e)) => {
                    problem(
                        DiagnosticKind::InvalidName,
                        "t-name",
                        at_attr("t-name"),
                        e.to_string(),
                    );
                    None
                }
                None => None,
            };
            let parent = match inherit.map(TemplateName::new) {
                Some(Ok(p)) => Some(p),
                Some(Err(e)) => {
                    problem(
                        DiagnosticKind::InvalidName,
                        "t-inherit",
                        at_attr("t-inherit"),
                        e.to_string(),
                    );
                    None
                }
                None => None,
            };
            if mode.is_some() && inherit.is_none() {
                problem(
                    DiagnosticKind::MissingCompanion,
                    "t-inherit-mode",
                    at_attr("t-inherit-mode"),
                    "t-inherit-mode is only allowed with t-inherit".to_owned(),
                );
            }
            let extension = mode == Some("extension");
            if let Some(other) = mode.filter(|m| !matches!(*m, "primary" | "extension")) {
                problem(
                    DiagnosticKind::InvalidDirectiveValue,
                    "t-inherit-mode",
                    at_attr("t-inherit-mode"),
                    format!("Invalid inherit mode '{other}': expected 'primary' or 'extension'"),
                );
            }
            if extension && name_attr.is_some() {
                problem(
                    DiagnosticKind::InvalidDirectiveValue,
                    "t-name",
                    at_attr("t-name"),
                    "an extension (t-inherit-mode=\"extension\") modifies its parent and has no t-name"
                        .to_owned(),
                );
            }
            if !extension && name_attr.is_none() {
                problem(
                    DiagnosticKind::FileShape,
                    "t-name",
                    top.range.start,
                    "Template name is missing.".to_owned(),
                );
            }
            if extension {
                // The extension's root renders nothing: its children are the specs (silent-failure
                // review M-b). A directive there is an error; a plain attribute is an error too,
                // except the markers OWL drops (a warning, like on any <t>).
                for a in &el.attrs {
                    if matches!(a.name, "t-name" | "t-inherit" | "t-inherit-mode") {
                        continue;
                    }
                    let offset = a.offset as usize;
                    if !a.name.starts_with("t-") && T_MARKERS.contains(&a.name) {
                        set_diags.push(
                            Diagnostic::new(
                                Severity::Warning,
                                DiagnosticKind::IgnoredAttribute,
                                format!(
                                    "attribute '{}' on an extension's root is ignored: only its children are applied",
                                    a.name
                                ),
                            )
                            .with_directive("attribute")
                            .with_location(file_loc(fi, offset)),
                        );
                    } else {
                        let directive = if a.name.starts_with("t-") {
                            a.name.split('.').next().unwrap_or(a.name)
                        } else {
                            "attribute"
                        };
                        problem(
                            DiagnosticKind::DirectiveNotAllowedHere,
                            directive,
                            offset,
                            format!(
                                "'{}' is not allowed on an extension's root: only its children are applied to the parent",
                                a.name
                            ),
                        );
                    }
                }
            }
            // A template whose t-inherit is invalid stays a root template, rejected by its error.
            let role = match (name, parent) {
                (_, Some(parent)) if extension => Some(Role::Extension { parent }),
                (Some(name), Some(parent)) if !extension => Some(Role::Primary { name, parent }),
                (Some(name), None) if !extension => Some(Role::Root { name }),
                _ => None,
            };
            let index = tops.len();
            if !problems.is_empty() {
                extra.insert(index, problems);
            }
            let spec_offsets = match &role {
                Some(Role::Primary { .. } | Role::Extension { .. }) => {
                    provenance::operation_offsets(&top.element)
                }
                // A root declaration has no inheritance operations.
                Some(Role::Root { .. }) | None => Vec::new(),
            };
            tops.push(Top {
                file: fi,
                top,
                role,
                location,
                spec_offsets,
            });
        }
    }

    // 3. Names, duplicates, parents.
    // `tops.len()` is at most the `templates` limit, so every index fits an `EntryId` (const
    // assertion at the top of this file).
    let id_of = |i: usize| i as EntryId;
    let mut by_name: BTreeMap<&TemplateName, EntryId> = BTreeMap::new();
    for (i, t) in tops.iter().enumerate() {
        let Some(name) = t.role.as_ref().and_then(Role::name) else {
            continue;
        };
        if let Some(&first) = by_name.get(name) {
            extra.entry(i).or_default().push(
                Diagnostic::error(
                    DiagnosticKind::DuplicateTemplate,
                    format!(
                        "template '{name}' is already defined at {}",
                        tops[first as usize].location
                    ),
                )
                .with_directive("t-name")
                .with_template(Some(name))
                .with_location(t.location.clone()),
            );
        } else {
            by_name.insert(name, id_of(i));
        }
    }
    let names: BTreeSet<TemplateName> = by_name.keys().map(|n| (*n).clone()).collect();
    let mut extended: BTreeSet<EntryId> = BTreeSet::new();
    let mut extensions = 0usize;
    for (i, t) in tops.iter().enumerate() {
        let Some(role) = &t.role else { continue };
        let Some(parent) = role.parent() else {
            continue;
        };
        let is_ext = role.is_extension();
        extensions += usize::from(is_ext);
        match by_name.get(parent) {
            Some(&p) if is_ext => {
                extended.insert(p);
            }
            Some(_) => {}
            None => {
                let message = format!(
                    "template parent '{parent}' not found (t-inherit of {})",
                    role.name()
                        .map_or_else(|| "an extension".to_owned(), |n| format!("'{n}'"))
                );
                extra.entry(i).or_default().push(
                    Diagnostic::error(DiagnosticKind::MissingTemplate, message)
                        .with_directive("t-inherit")
                        .with_template(role.name().or_else(|| role.parent()))
                        .with_location(t.location.clone()),
                );
            }
        }
    }
    // Extensions with errors of their own (an invalid name or attribute, a missing parent): their
    // errors are set diagnostics, and they are left out of the inheritance, so their parent — and
    // every primary child built on it — fails instead of compiling without them (silent-failure
    // review H1).
    let mut broken: BTreeSet<EntryId> = BTreeSet::new();
    let mut failing: BTreeMap<EntryId, Vec<String>> = BTreeMap::new();
    for (i, t) in tops.iter().enumerate() {
        let is_ext = match &t.role {
            Some(role) => role.is_extension(),
            // Neither a template nor an extension could be read: its problems are set-level.
            None => true,
        };
        if !is_ext {
            continue;
        }
        if let Some(problems) = extra.remove(&i) {
            set_diags.extend(problems);
            broken.insert(id_of(i));
            if let Some(&p) = t
                .role
                .as_ref()
                .and_then(Role::parent)
                .and_then(|p| by_name.get(p))
            {
                failing
                    .entry(p)
                    .or_default()
                    .push(format!("the extension at {}", t.location));
            }
        }
    }
    // Primary children of a failing template fail too, to a fixpoint (a parent chain is finite:
    // each template is added once).
    loop {
        let mut added = Vec::new();
        for (i, t) in tops.iter().enumerate() {
            if let Some(Role::Primary { parent, .. }) = &t.role
                && let Some(&p) = by_name.get(parent)
                && failing.contains_key(&p)
                && !failing.contains_key(&id_of(i))
            {
                added.push((id_of(i), format!("its parent '{parent}'")));
            }
        }
        if added.is_empty() {
            break;
        }
        for (i, why) in added {
            failing.entry(i).or_default().push(why);
        }
    }
    for (&i, why) in &failing {
        let t = &tops[i as usize];
        extra.entry(i as usize).or_default().push(
            Diagnostic::error(
                DiagnosticKind::Inheritance,
                format!(
                    "the template is not compiled without its inheritance: {} has errors",
                    why.join(", ")
                ),
            )
            .with_directive("t-inherit")
            .with_template(t.role.as_ref().and_then(Role::name))
            .with_location(t.location.clone()),
        );
    }

    // One linear position pass per original file; candidate lookup never rescans a long line
    // for every operation or inserted node.
    let mut source_builder = provenance::Builder::with_budget(
        limits
            .get(LimitKind::InputBytes)
            .min(provenance::STORAGE_BYTES),
    );
    if translation
        .as_ref()
        .is_some_and(|pass| pass.tracks_literal_ownership())
    {
        source_builder = source_builder.track_translation_ownership();
    }
    let mut source_positions = Vec::with_capacity(files.len());
    for (file, (roots, _)) in files.iter().zip(&parsed) {
        match source_builder.positions(file.xml(), roots) {
            Ok(positions) => source_positions.push(positions),
            Err(error) => {
                set_diags.push(error.diagnostic());
                return Run::stopped(files.len(), set_diags);
            }
        }
    }
    // One bounded original-source index, shared by every combined template's diagnostics.
    for (i, top) in tops.iter().enumerate() {
        if let Err(error) = source_builder.add(provenance::Input {
            entry: id_of(i),
            root: &top.top.element,
            file: &files[top.file],
            positions: &source_positions[top.file],
            specs: &top.spec_offsets,
        }) {
            set_diags.push(error.diagnostic());
            return Run::stopped(files.len(), set_diags);
        }
    }
    let source_index = source_builder.finish();
    drop(source_positions);

    // 4. Inheritance through erp-views.
    let entries: Vec<Entry<'_>> = tops
        .iter()
        .map(|t| {
            let f = &files[t.file];
            let src = f.xml();
            Entry {
                role: t.role.as_ref(),
                trust: f.kind().trust(),
                element: &src[t.top.range.clone()],
                inner: &src[t.top.inner.clone()],
            }
        })
        .collect();
    let targets: Vec<EntryId> = tops
        .iter()
        .enumerate()
        .filter(|(i, t)| {
            !extra.contains_key(i)
                && match &t.role {
                    Some(Role::Primary { .. }) => true,
                    Some(Role::Root { .. }) => extended.contains(&id_of(*i)),
                    Some(Role::Extension { .. }) | None => false,
                }
        })
        .map(|(i, _)| id_of(i))
        .collect();
    let results = inherit::combine(&entries, &by_name, &targets, &broken, view_limits, limits);
    let mut combined: BTreeMap<usize, Result<inherit::Combined, inherit::CombineError>> =
        BTreeMap::new();
    if results.len() == targets.len() {
        combined.extend(targets.iter().map(|&t| t as usize).zip(results));
    } else {
        set_diags.push(Diagnostic::internal(format!(
            "the inheritance adapter returned {} results for {} templates",
            results.len(),
            targets.len()
        )));
        for &t in &targets {
            extra
                .entry(t as usize)
                .or_default()
                .push(Diagnostic::internal("template inheritance was not applied"));
        }
    }

    // 5. Analyse every template.
    let mut parser = ExprParser::new(limits);
    let mut reports: Vec<TemplateReport> = Vec::new();
    let mut analysed: Vec<(usize, Analysed)> = Vec::new();
    for (i, t) in tops.iter().enumerate() {
        let Some(name) = t.role.as_ref().and_then(Role::name) else {
            // Extensions and unreadable entries: their problems are set diagnostics already.
            continue;
        };
        let name = name.clone();
        let f = &files[t.file];
        let mut diagnostics = extra.remove(&i).unwrap_or_default();
        let mut result = None;
        if diagnostics.is_empty() {
            match combined.remove(&i) {
                None => {
                    let loc = LocCtx {
                        origin: f.origin(),
                        text: f.xml(),
                        lines: Some(&parsed[t.file].1),
                    };
                    let analysis = (|| {
                        let translated = translation
                            .as_deref_mut()
                            .map(|pass| pass.transform(&t.top.element, &loc, None))
                            .transpose()?;
                        // No pass means the existing source-mode tree is used unchanged.
                        let root = match &translated {
                            Some(root) => root,
                            None => &t.top.element,
                        };
                        Ok::<_, Diagnostic>(analyze_template(
                            &mut parser,
                            options,
                            f.kind().trust(),
                            &names,
                            &name,
                            root,
                            &loc,
                        ))
                    })();
                    match analysis {
                        Ok(a) => result = Some((a, AnalysedText::File)),
                        Err(error) => diagnostics.push(error.with_template(Some(&name))),
                    }
                }
                Some(Err(e)) => {
                    let detail = if let inherit::CombineProblem::DuplicateAnchor {
                        name,
                        first_tag,
                        first_origin,
                        second_tag,
                        second_origin,
                    } = &e.error
                    {
                        let origin = |view_id| {
                            inherit::entry_index(view_id)
                                .and_then(|entry| tops.get(entry as usize))
                                .map(|top| top.location.to_string())
                        };
                        match (
                            origin(first_origin.view_id()),
                            origin(second_origin.view_id()),
                        ) {
                            (Some(first), Some(second)) => format!(
                                "duplicate t-anchor '{name}': <{first_tag}> from {first} (entry line {}) and <{second_tag}> from {second} (entry line {}) declare the same anchor",
                                first_origin.line(),
                                second_origin.line(),
                            ),
                            // The adapter's ids must point at entries; make an invariant failure
                            // visible instead of losing either origin silently.
                            _ => format!(
                                "internal compiler error: duplicate anchor origins are outside the template set: {}",
                                e.error.message()
                            ),
                        }
                    } else {
                        e.error.message()
                    };
                    let mut message = format!("template inheritance failed: {detail}");
                    let mut kind = DiagnosticKind::Inheritance;
                    let location = match (e.entry, e.error.spec_index()) {
                        (Some(entry), Some(index)) => {
                            let source = tops.get(entry as usize);
                            match source.and_then(|source| {
                                index
                                    .checked_sub(1)
                                    .and_then(|i| source.spec_offsets.get(i))
                                    .map(|&offset| (source.file, offset))
                            }) {
                                Some((file, offset)) => file_loc(file, offset as usize),
                                None => {
                                    kind = DiagnosticKind::Internal;
                                    message = format!(
                                        "internal compiler error: inheritance spec {index} has no original source offset: {detail}"
                                    );
                                    t.location.clone()
                                }
                            }
                        }
                        // Declaration, graph and root-only failures have no spec ordinal.
                        (entry, None) => entry.map_or_else(
                            || t.location.clone(),
                            |entry| tops[entry as usize].location.clone(),
                        ),
                        (None, Some(index)) => {
                            kind = DiagnosticKind::Internal;
                            message = format!(
                                "internal compiler error: inheritance spec {index} has no source entry: {detail}"
                            );
                            t.location.clone()
                        }
                    };
                    diagnostics.push(
                        Diagnostic::error(kind, message)
                            .with_directive("t-inherit")
                            .with_template(Some(&name))
                            .with_location(location),
                    );
                }
                Some(Ok(c)) => {
                    let note = combined_note(&c.applied, &tops, files);
                    let analysis = match xml::parse_element(&c.xml, limits) {
                        Err(p) => {
                            let d = match p.limit {
                                Some(hit) => Diagnostic::limit_hit(hit),
                                None => Diagnostic::error(p.kind, p.message),
                            };
                            diagnostics.push(
                                d.with_note(&note)
                                    .with_template(Some(&name))
                                    .with_location(t.location.clone()),
                            );
                            None
                        }
                        Ok(root) => match source_index.map(&root, &c.xml, &c.origins, &c.applied) {
                            Ok(provenance) => {
                                let loc = LocCtx {
                                    origin: f.origin(),
                                    text: &c.xml,
                                    lines: None,
                                };
                                let analysis = (|| {
                                    let translated = translation
                                        .as_deref_mut()
                                        .map(|pass| pass.transform(&root, &loc, Some(&provenance)))
                                        .transpose()?;
                                    // Source-mode analysis retains the original combined tree.
                                    let root = match &translated {
                                        Some(root) => root,
                                        None => &root,
                                    };
                                    Ok::<_, Diagnostic>(analyze_template(
                                        &mut parser,
                                        options,
                                        c.trust,
                                        &names,
                                        &name,
                                        root,
                                        &loc,
                                    ))
                                })();
                                match analysis {
                                    Ok(a) => Some((a, provenance)),
                                    Err(error) => {
                                        diagnostics.push(error.with_template(Some(&name)));
                                        None
                                    }
                                }
                            }
                            Err(error) => {
                                diagnostics.push(error.diagnostic().with_template(Some(&name)));
                                None
                            }
                        },
                    };
                    result = analysis.map(|(a, provenance)| {
                        (
                            a,
                            AnalysedText::Combined {
                                xml: c.xml,
                                provenance,
                            },
                        )
                    });
                }
            }
        }
        let report_index = reports.len();
        if let Some((a, text)) = result {
            diagnostics.extend(a.diagnostics.iter().cloned());
            analysed.push((
                report_index,
                Analysed {
                    file: t.file,
                    text,
                    location: t.location.clone(),
                    analysis: a,
                },
            ));
        }
        reports.push(TemplateReport {
            name,
            location: t.location.clone(),
            verdict: Verdict::Passed,
            diagnostics,
        });
    }
    // Anything left in `extra` belongs to no template that was reported: keep it.
    set_diags.extend(extra.into_values().flatten());

    // 6. Callee templates: free names through the static call graph (design note §6.4).
    free_names(&mut reports, &analysed);
    check_cycles(&mut reports, &analysed);
    // Resolve every combined diagnostic only once, including graph diagnostics created above.
    // An unresolved/ambiguous origin is explicitly a candidate list, never the root's source.
    for (index, analysed) in &analysed {
        if let AnalysedText::Combined { provenance, .. } = &analysed.text {
            reports[*index].diagnostics = std::mem::take(&mut reports[*index].diagnostics)
                .into_iter()
                .map(|d| provenance.remap(d))
                .collect();
        }
    }

    // 7. The translation pass is: say so once per set that has text to translate
    // (silent-failure review M-e).
    if translation.is_none() && analysed.iter().any(|(_, a)| a.analysis.translatable) {
        set_diags.push(
            Diagnostic::new(
                Severity::Info,
                DiagnosticKind::Skipped,
                format!(
                    "translation pass skipped: static text and .translate values are emitted untranslated until {UI3_DONE}"
                ),
            )
            .with_directive("translation"),
        );
    }

    for r in &mut reports {
        if r.diagnostics
            .iter()
            .any(|d| d.severity() == Severity::Error)
        {
            r.verdict = Verdict::Rejected;
        }
    }
    let analysed = analysed
        .into_iter()
        .filter(|(ri, _)| reports[*ri].verdict == Verdict::Passed)
        .map(|(_, a)| a)
        .collect();
    drop(tops);
    let lines = parsed.into_iter().map(|(_, lines)| lines).collect();
    Run {
        analysis: Analysis {
            files: files.len(),
            extensions,
            templates: reports,
            set_diagnostics: set_diags,
        },
        analysed,
        lines,
    }
}

/// The note appended to a combined template's diagnostics (which carry no position): the
/// sources of the entries applied, in order (rust review L1).
fn combined_note(applied: &[EntryId], tops: &[Top<'_>], files: &[TemplateSource]) -> String {
    let mut origins: Vec<String> = Vec::new();
    for &e in applied {
        // `combine` returns ids of `tops`; a stray one is named, never skipped.
        let origin = match tops.get(e as usize).and_then(|t| files.get(t.file)) {
            Some(file) => file.origin().to_string(),
            None => format!("unknown entry {e} (internal compiler error)"),
        };
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    format!(
        " (in the combined template built from {})",
        origins.join(", ")
    )
}

/// Propagates free names along static `t-call`s and reports the ones no caller binds.
fn free_names(reports: &mut [TemplateReport], analysed: &[(usize, Analysed)]) {
    let index: BTreeMap<&str, usize> = analysed
        .iter()
        .enumerate()
        .map(|(i, (_, a))| (a.analysis.ir.name.as_str(), i))
        .collect();
    // free[i]: names template i needs from its callers, with the chain they come through.
    let mut free: Vec<BTreeMap<Box<str>, Vec<usize>>> = analysed
        .iter()
        .map(|(_, a)| {
            a.analysis
                .free
                .keys()
                .map(|k| (k.clone(), Vec::new()))
                .collect()
        })
        .collect();
    let mut called: BTreeSet<usize> = BTreeSet::new();
    for (_, a) in analysed {
        for site in &a.analysis.calls {
            if let Some(&c) = index.get(site.callee.as_str()) {
                called.insert(c);
            }
        }
    }
    // Fixpoint: a caller needs its callees' free names that the call site does not bind.
    loop {
        let mut changed = false;
        for (i, (_, a)) in analysed.iter().enumerate() {
            for site in &a.analysis.calls {
                let Some(&c) = index.get(site.callee.as_str()) else {
                    continue;
                };
                let needed: Vec<(Box<str>, Vec<usize>)> = free[c]
                    .iter()
                    .filter(|(n, _)| !site.binds(n))
                    .map(|(n, chain)| {
                        let mut chain2 = vec![c];
                        chain2.extend(chain.iter().copied());
                        (n.clone(), chain2)
                    })
                    .collect();
                for (n, chain) in needed {
                    if let std::collections::btree_map::Entry::Vacant(slot) = free[i].entry(n) {
                        slot.insert(chain);
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    for (i, (ri, a)) in analysed.iter().enumerate() {
        if called.contains(&i) {
            continue;
        }
        for (n, chain) in &free[i] {
            let (reader, location) = match chain.last() {
                None => (a, a.analysis.free.get(n).cloned()),
                Some(&last) => (
                    &analysed[last].1,
                    analysed[last].1.analysis.free.get(n).cloned(),
                ),
            };
            let mut path = vec![a.analysis.ir.name.as_str()];
            path.extend(
                chain
                    .iter()
                    .map(|&c| analysed[c].1.analysis.ir.name.as_str()),
            );
            let message = if chain.is_empty() {
                format!(
                    "unknown identifier '{n}': not a template local, an allowed global or 'this.' (use this.{n} for a component member)"
                )
            } else {
                format!(
                    "unknown identifier '{n}' read by template '{}' through t-call {}: no caller binds it",
                    reader.analysis.ir.name,
                    path.join(" -> ")
                )
            };
            let mut d = Diagnostic::error(DiagnosticKind::UnknownIdentifier, message)
                .with_directive("expression")
                .with_template(Some(&a.analysis.ir.name));
            if let Some(location) = location {
                d = d.with_location(location);
            }
            reports[*ri].diagnostics.push(d);
        }
    }
}

/// Detects static call cycles without an intervening `t-if` guard.
fn check_cycles(reports: &mut [TemplateReport], analysed: &[(usize, Analysed)]) {
    let index: BTreeMap<&str, usize> = analysed
        .iter()
        .enumerate()
        .map(|(i, (_, a))| (a.analysis.ir.name.as_str(), i))
        .collect();

    let mut graph: Vec<Vec<(usize, Pos)>> = vec![Vec::new(); analysed.len()];
    for (i, (_, a)) in analysed.iter().enumerate() {
        for site in &a.analysis.calls {
            if !site.guarded
                && let Some(&callee_idx) = index.get(site.callee.as_str())
            {
                graph[i].push((callee_idx, site.pos));
            }
        }
    }

    let mut visited = vec![false; analysed.len()];
    let mut on_stack = vec![false; analysed.len()];
    let mut path = Vec::new();

    fn dfs(
        u: usize,
        graph: &[Vec<(usize, Pos)>],
        visited: &mut [bool],
        on_stack: &mut [bool],
        path: &mut Vec<(usize, Pos)>,
        analysed: &[(usize, Analysed)],
        reports: &mut [TemplateReport],
    ) {
        visited[u] = true;
        on_stack[u] = true;

        for &(v, pos) in &graph[u] {
            path.push((u, pos));
            if on_stack[v] {
                let cycle_start = path.iter().position(|&(node, _)| node == v).unwrap_or(0);
                let cycle_nodes: Vec<&str> = path[cycle_start..]
                    .iter()
                    .map(|&(node, _)| analysed[node].1.analysis.ir.name.as_str())
                    .chain(std::iter::once(analysed[v].1.analysis.ir.name.as_str()))
                    .collect();
                let message = format!(
                    "static call cycle detected: {} without intervening t-if guard",
                    cycle_nodes.join(" -> ")
                );
                let (ri, a) = &analysed[u];
                reports[*ri].diagnostics.push(
                    Diagnostic::error(DiagnosticKind::Cycle, message)
                        .with_directive("t-call")
                        .with_template(Some(&a.analysis.ir.name)),
                );
            } else if !visited[v] {
                dfs(v, graph, visited, on_stack, path, analysed, reports);
            }
            path.pop();
        }

        on_stack[u] = false;
    }

    for i in 0..analysed.len() {
        if !visited[i] {
            dfs(
                i,
                &graph,
                &mut visited,
                &mut on_stack,
                &mut path,
                analysed,
                reports,
            );
        }
    }
}

/// The addon guide's template page, whose Rust example is
/// compiled and run as a doctest so the guide cannot drift from the API.
#[cfg(doctest)]
mod addon_guide_templates {}
