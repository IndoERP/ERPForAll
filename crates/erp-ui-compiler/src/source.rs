//! Compiler inputs: template sources (whose kind fixes the trust mode of their expressions,
//! T-TPL.1), the ordered template set, and the compile options.

use crate::limits::CompilerLimits;
use crate::names::{AddonName, DirectiveName, TemplatePath};
use std::fmt;

/// Where a template source comes from. The kind decides the trust mode: there is no mode flag a
/// caller could set wrongly (threat model T-TPL.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SourceKind {
    /// An addon's `static/src/**/*.xml` file read at bundle assembly: trusted mode.
    AddonFile,
}

impl SourceKind {
    /// The trust mode templates of this kind are analysed in.
    pub(crate) fn trust(self) -> Trust {
        match self {
            SourceKind::AddonFile => Trust::Trusted,
        }
    }
}

/// The trust mode a template is analysed in (design note §6.4). It follows the
/// source of every part of a template: an inherited template takes the least trusted mode of the
/// entries combined into it (`Trust::least`). Restricted mode (database-stored templates) is added
/// together with its own rules; it is never a compile option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Trust {
    /// Addon files: the full expression grammar of §6.2, `this` is the component.
    Trusted,
}

impl Trust {
    /// The less trusted of two modes. An exhaustive match, so a new mode (restricted,
    /// ) must state how it combines (round-two review L3).
    pub(crate) fn least(self, other: Trust) -> Trust {
        match (self, other) {
            (Trust::Trusted, Trust::Trusted) => Trust::Trusted,
        }
    }
}

/// Where a template source, and so a diagnostic, comes from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Origin {
    /// A file of an addon.
    AddonFile {
        /// The addon.
        addon: AddonName,
        /// The file's path inside the addon.
        path: TemplatePath,
    },
}

impl Origin {
    /// The source kind of this origin.
    #[must_use]
    pub fn kind(&self) -> SourceKind {
        match self {
            Origin::AddonFile { .. } => SourceKind::AddonFile,
        }
    }

    /// The addon owning this origin.
    #[must_use]
    pub fn addon(&self) -> &AddonName {
        match self {
            Origin::AddonFile { addon, .. } => addon,
        }
    }

    /// The template path within the addon.
    #[must_use]
    pub fn path(&self) -> &TemplatePath {
        match self {
            Origin::AddonFile { path, .. } => path,
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::AddonFile { addon, path } => write!(f, "{addon}/{path}"),
        }
    }
}

/// One template source file.
///
/// ```
/// use erp_ui_compiler::{AddonName, Origin, SourceKind, TemplatePath, TemplateSource};
///
/// let src = TemplateSource::addon_file(
///     AddonName::new("web")?,
///     TemplatePath::new("static/src/x.xml")?,
///     "<templates><t t-name=\"web.X\"><div/></t></templates>",
/// );
/// assert_eq!(src.kind(), SourceKind::AddonFile);
/// assert!(matches!(src.origin(), Origin::AddonFile { path, .. } if path.as_str() == "static/src/x.xml"));
/// # Ok::<(), erp_ui_compiler::NameError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSource {
    origin: Origin,
    xml: Box<str>,
}

impl TemplateSource {
    /// An addon template file. `path` is the file's path inside the addon (used in diagnostics
    /// only; it never reaches generated code), `xml` its content.
    #[must_use]
    pub fn addon_file(addon: AddonName, path: TemplatePath, xml: impl Into<Box<str>>) -> Self {
        Self {
            origin: Origin::AddonFile { addon, path },
            xml: xml.into(),
        }
    }

    /// The source kind.
    #[must_use]
    pub fn kind(&self) -> SourceKind {
        self.origin.kind()
    }

    /// Where the source comes from.
    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// The XML text.
    #[must_use]
    pub fn xml(&self) -> &str {
        &self.xml
    }
}

/// The template files of one addon set, in compile order: load order, then the order of
/// the files inside each addon. Templates and extensions get their synthetic ids in this order
/// (design note §5), so the order is part of the input.
///
/// ```
/// use erp_ui_compiler::{AddonName, TemplatePath, TemplateSet, TemplateSource};
///
/// let mut set = TemplateSet::new();
/// set.push(TemplateSource::addon_file(
///     AddonName::new("web")?,
///     TemplatePath::new("a.xml")?,
///     "<templates/>",
/// ));
/// assert_eq!(set.len(), 1);
/// # Ok::<(), erp_ui_compiler::NameError>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplateSet {
    files: Vec<TemplateSource>,
}

impl TemplateSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a file after the ones already in the set.
    pub fn push(&mut self, source: TemplateSource) {
        self.files.push(source);
    }

    /// The files, in order.
    #[must_use]
    pub fn files(&self) -> &[TemplateSource] {
        &self.files
    }

    /// Number of files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the set has no files.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Output mode: part of every cache key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BuildMode {
    /// Production bundles: `t-debug`/`t-log` and `t-anchor` stripped, `console` not a global.
    Prod,
    /// Development bundles (`erp dev`).
    Dev,
}

pub const MAX_CUSTOM_DIRECTIVES: usize = 256;

/// Options of one compile: everything besides the sources that the output depends on, so all of it
/// is part of a cache key (`Hash` for that).
///
/// ```
/// use erp_ui_compiler::{BuildMode, CompileOptions};
///
/// let options = CompileOptions::new(BuildMode::Prod).with_custom_directive("click")?;
/// let names: Vec<&str> = options.custom_directives().iter().map(|n| n.as_str()).collect();
/// assert_eq!(names, ["click"]);
/// # Ok::<(), erp_ui_compiler::NameError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompileOptions {
    mode: BuildMode,
    limits: CompilerLimits,
    custom_directives: Vec<DirectiveName>,
}

impl CompileOptions {
    /// Options with the default limits and no custom directive.
    #[must_use]
    pub fn new(mode: BuildMode) -> Self {
        Self {
            mode,
            limits: CompilerLimits::DEFAULT,
            custom_directives: Vec::new(),
        }
    }

    /// Sets the limits.
    #[must_use]
    pub fn with_limits(mut self, limits: CompilerLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Registers a custom directive name (`t-custom-<name>`), as addons do in `register(ctx)`
    /// (design §4.1). Names are kept sorted and unique; registering a name twice is a no-op.
    ///
    /// # Errors
    /// [`crate::NameError::Directive`] when the name is not a [`DirectiveName`];
    /// [`crate::NameError::TooManyDirectives`] past [`MAX_CUSTOM_DIRECTIVES`] names.
    pub fn with_custom_directive(mut self, name: &str) -> Result<Self, crate::NameError> {
        let name = DirectiveName::new(name)?;
        if let Err(at) = self.custom_directives.binary_search(&name) {
            if self.custom_directives.len() >= MAX_CUSTOM_DIRECTIVES {
                return Err(crate::NameError::TooManyDirectives {
                    name: name.as_str().to_owned(),
                    max: MAX_CUSTOM_DIRECTIVES,
                });
            }
            self.custom_directives.insert(at, name);
        }
        Ok(self)
    }

    /// The build mode.
    #[must_use]
    pub fn mode(&self) -> BuildMode {
        self.mode
    }

    /// The limits.
    #[must_use]
    pub fn limits(&self) -> &CompilerLimits {
        &self.limits
    }

    /// The registered custom directive names, sorted.
    #[must_use]
    pub fn custom_directives(&self) -> &[DirectiveName] {
        &self.custom_directives
    }

    pub(crate) fn has_custom_directive(&self, name: &str) -> bool {
        self.custom_directives
            .binary_search_by(|probe| probe.as_str().cmp(name))
            .is_ok()
    }
}
