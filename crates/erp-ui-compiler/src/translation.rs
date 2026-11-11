//! Pure, bounded semantic template translation. Catalogue data is never executable
//! template source: only selected literal values change before the ordinary serializers run.

use crate::analyze::LocCtx;
use crate::diag::{Diagnostic, DiagnosticKind, Location};
use crate::limits::{CompilerLimits, LimitKind};
use crate::names::AddonName;
use crate::provenance::Mapping;
use crate::xml::{XElement, XNode, is_xml_space};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

/// Hard term count admission for one immutable catalogue or extraction result.
pub const MAX_TRANSLATION_TERMS: usize = 100_000;
/// Hard UTF-8 byte admission for a source or translated literal.
pub const MAX_TRANSLATION_TERM_BYTES: usize = 65_536;
/// Portable charged storage admission; this is not a measured heap size.
pub const MAX_TRANSLATION_BYTES: usize = 16_777_216;
const TERM_CHARGE: usize = 128;
const OCCURRENCE_CHARGE: usize = 128;

/// Why a catalogue could not be constructed. Malformed data is never an absent catalogue.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TranslationError {
    /// Language identity is empty, too long or has malformed components.
    #[error(
        "invalid translation language: expected 1..64 ASCII letters/digits separated by '_' or '-'"
    )]
    InvalidLanguage,
    /// An input or aggregate admission limit was exceeded.
    #[error("translation {subject} exceeds {max} {unit}")]
    Limit {
        /// Bounded input subject, such as source, value or catalogue.
        subject: &'static str,
        /// Hard maximum.
        max: usize,
        /// Unit of the limit.
        unit: &'static str,
    },
    /// A source key or translated literal cannot round-trip NUL through HTML parsing.
    #[error("translation {subject} contains a NUL byte at offset {offset}")]
    Nul {
        /// Input subject: source or value.
        subject: &'static str,
        /// Zero-based UTF-8 byte offset of the first NUL.
        offset: usize,
    },
    /// One module/source key was supplied with two different translations.
    #[error("conflicting translation for addon '{addon}', source '{term}'")]
    ConflictingTerm {
        /// Module identity, or the explicit global scope.
        addon: Box<str>,
        /// Bounded source literal.
        term: Box<str>,
    },
}

/// Validated immutable per-language catalogue, independent of installed database languages.
/// Module entries override global entries; missing terms retain the source literal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TranslationCatalogue {
    language: Box<str>,
    modules: BTreeMap<AddonName, BTreeMap<Box<str>, Box<str>>>,
    term_count: usize,
    global: BTreeMap<Box<str>, Box<str>>,
    charged: usize,
}

impl TranslationCatalogue {
    /// Construct an empty catalogue with an explicit language identity.
    ///
    /// # Errors
    /// [`TranslationError::InvalidLanguage`] unless the identity contains 1..64 ASCII
    /// letters/digits in nonempty components separated by `_` or `-`.
    pub fn new(language: &str) -> Result<Self, TranslationError> {
        if language.is_empty()
            || language.len() > 64
            || !language.split(['_', '-']).all(|part| {
                !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
        {
            return Err(TranslationError::InvalidLanguage);
        }
        Ok(Self {
            language: language.into(),
            modules: BTreeMap::new(),
            term_count: 0,
            global: BTreeMap::new(),
            charged: language.len(),
        })
    }

    /// Add a module-scoped literal. Identical repeated declarations are idempotent.
    ///
    /// # Errors
    /// [`TranslationError::Limit`] for string/count/charged-byte admission,
    /// [`TranslationError::Nul`] for a NUL in the source key or translated value, or
    /// [`TranslationError::ConflictingTerm`] for a different value at the same key.
    /// NUL is refused before duplicate handling or insertion, never stripped or replaced.
    pub fn with_term(
        mut self,
        addon: AddonName,
        source: &str,
        translated: &str,
    ) -> Result<Self, TranslationError> {
        validate_strings(source, translated)?;
        // Nested borrowed lookups avoid copying source keys and bound lookup work logarithmically.
        if let Some(previous) = self.modules.get(&addon).and_then(|terms| terms.get(source)) {
            if previous.as_ref() == translated {
                return Ok(self);
            }
            return Err(TranslationError::ConflictingTerm {
                addon: addon.as_str().into(),
                term: source.into(),
            });
        }
        self.admit(addon.as_str().len(), source.len(), translated.len())?;
        self.modules
            .entry(addon)
            .or_default()
            .insert(source.into(), translated.into());
        Ok(self)
    }

    /// Add a global fallback literal. Module-scoped entries always take precedence.
    ///
    /// # Errors
    /// [`TranslationError::Limit`] for string/count/charged-byte admission,
    /// [`TranslationError::Nul`] for a NUL in the source key or translated value, or
    /// [`TranslationError::ConflictingTerm`] for a different value at the same key.
    /// NUL is refused before duplicate handling or insertion, never stripped or replaced.
    pub fn with_global_term(
        mut self,
        source: &str,
        translated: &str,
    ) -> Result<Self, TranslationError> {
        validate_strings(source, translated)?;
        if let Some(previous) = self.global.get(source) {
            if previous.as_ref() == translated {
                return Ok(self);
            }
            return Err(TranslationError::ConflictingTerm {
                addon: "global".into(),
                term: source.into(),
            });
        }
        self.admit(0, source.len(), translated.len())?;
        self.global.insert(source.into(), translated.into());
        Ok(self)
    }

    /// Language identity of this immutable catalogue; not installation authorization.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }

    fn admit(
        &mut self,
        addon_bytes: usize,
        source_bytes: usize,
        value_bytes: usize,
    ) -> Result<(), TranslationError> {
        let Some(next_count) = self
            .term_count
            .checked_add(1)
            .filter(|count| *count <= MAX_TRANSLATION_TERMS)
        else {
            return Err(TranslationError::Limit {
                subject: "catalogue",
                max: MAX_TRANSLATION_TERMS,
                unit: "terms",
            });
        };
        let next = self
            .charged
            .checked_add(TERM_CHARGE)
            .and_then(|used| used.checked_add(addon_bytes))
            .and_then(|used| used.checked_add(source_bytes))
            .and_then(|used| used.checked_add(value_bytes));
        let Some(next) = next.filter(|used| *used <= MAX_TRANSLATION_BYTES) else {
            return Err(TranslationError::Limit {
                subject: "catalogue",
                max: MAX_TRANSLATION_BYTES,
                unit: "charged bytes",
            });
        };
        self.charged = next;
        // The count was checked before any mutation; it cannot exceed the hard maximum.
        self.term_count = next_count;
        Ok(())
    }

    fn lookup<'a>(&'a self, addon: &AddonName, source: &'a str) -> &'a str {
        // Missing terms deliberately fall back module -> global -> source.
        self.modules
            .get(addon)
            .and_then(|terms| terms.get(source))
            .map(|value| value.as_ref())
            .or_else(|| self.global.get(source).map(|value| value.as_ref()))
            .unwrap_or(source)
    }
}

fn validate_strings(source: &str, translated: &str) -> Result<(), TranslationError> {
    for (subject, value) in [("source", source), ("value", translated)] {
        if value.len() > MAX_TRANSLATION_TERM_BYTES {
            return Err(TranslationError::Limit {
                subject,
                max: MAX_TRANSLATION_TERM_BYTES,
                unit: "bytes",
            });
        }
    }
    // Bound both inputs before scanning character content; offsets are UTF-8 bytes, matching
    // the admission unit. NUL cannot round-trip through the destination HTML parser, while
    // supported Unicode and whitespace retain their existing literal semantics.
    for (subject, value) in [("source", source), ("value", translated)] {
        if let Some(offset) = value.find('\0') {
            return Err(TranslationError::Nul { subject, offset });
        }
    }
    Ok(())
}

/// One selected source literal, grouped by effective module and source text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TranslationTerm {
    addon: AddonName,
    source: Box<str>,
    locations: Vec<Location>,
}
impl TranslationTerm {
    /// Effective module used for lookup, including an explicit lexical context.
    #[must_use]
    pub fn addon(&self) -> &AddonName {
        &self.addon
    }
    /// Exact catalogue key, with only XML boundary whitespace removed for text nodes.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
    /// Original source occurrences, sorted and deduplicated.
    #[must_use]
    pub fn locations(&self) -> &[Location] {
        &self.locations
    }
}

/// Deterministic immutable extraction result. An erroneous template set returns no partial result.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExtractedTranslations {
    terms: Vec<TranslationTerm>,
}
impl ExtractedTranslations {
    /// Terms sorted by effective module and source text.
    #[must_use]
    pub fn terms(&self) -> &[TranslationTerm] {
        &self.terms
    }
}

pub(crate) struct Pass<'a> {
    catalogue: Option<&'a TranslationCatalogue>,
    /// Source admission validates/consumes new context declarations without changing literals.
    validation_only: bool,
    terms: BTreeMap<AddonName, BTreeMap<Box<str>, BTreeSet<Location>>>,
    term_count: usize,
    charged: usize,
    max_bytes: usize,
    tripped: bool,
}

impl<'a> Pass<'a> {
    pub(crate) fn new(
        catalogue: Option<&'a TranslationCatalogue>,
        limits: &CompilerLimits,
    ) -> Self {
        Self {
            catalogue,
            validation_only: false,
            terms: BTreeMap::new(),
            term_count: 0,
            charged: 0,
            max_bytes: MAX_TRANSLATION_BYTES.min(limits.get(LimitKind::OutputBytes)),
            tripped: false,
        }
    }

    pub(crate) fn validation(limits: &CompilerLimits) -> Self {
        let mut pass = Self::new(None, limits);
        pass.validation_only = true;
        pass
    }

    pub(crate) fn tracks_literal_ownership(&self) -> bool {
        !self.validation_only
    }

    pub(crate) fn finish(self) -> ExtractedTranslations {
        ExtractedTranslations {
            terms: self
                .terms
                .into_iter()
                .flat_map(|(addon, terms)| {
                    terms
                        .into_iter()
                        .map(move |(source, locations)| TranslationTerm {
                            addon: addon.clone(),
                            source,
                            locations: locations.into_iter().collect(),
                        })
                })
                .collect(),
        }
    }

    fn charge(&mut self, amount: usize) -> Result<(), Diagnostic> {
        let next = self.charged.checked_add(amount);
        if !self.tripped
            && let Some(next) = next.filter(|used| *used <= self.max_bytes)
        {
            self.charged = next;
            return Ok(());
        }
        self.tripped = true;
        Err(Diagnostic::over(
            LimitKind::OutputBytes,
            self.max_bytes,
            self.max_bytes.saturating_add(1),
        )
        .with_directive("translation"))
    }

    fn charge_at(&mut self, amount: usize, location: &Location) -> Result<(), Diagnostic> {
        self.charge(amount)
            .map_err(|diagnostic| diagnostic.with_location(location.clone()))
    }

    fn literal(
        &mut self,
        addon: &AddonName,
        source: &str,
        location: Location,
    ) -> Result<Option<String>, Diagnostic> {
        if source.len() > MAX_TRANSLATION_TERM_BYTES {
            return Err(Diagnostic::error(
                DiagnosticKind::InvalidDirectiveValue,
                "translation source exceeds 65536 bytes",
            )
            .with_directive("translation")
            .with_location(location));
        }
        // Entry keys are copied only after their own storage is admitted. Occurrences are
        // charged separately, so repeated source literals cannot evade the byte admission.
        let existing = self.terms.get(addon).and_then(|terms| terms.get(source));
        let new_key = existing.is_none();
        let new_location = existing.is_none_or(|locations| !locations.contains(&location));
        let next_count = self.term_count.checked_add(usize::from(new_key));
        let Some(next_count) = next_count.filter(|count| *count <= MAX_TRANSLATION_TERMS) else {
            return Err(Diagnostic::error(
                DiagnosticKind::InvalidDirectiveValue,
                "translation extraction exceeds 100000 terms",
            )
            .with_directive("translation")
            .with_location(location));
        };
        if new_key {
            self.charge_at(TERM_CHARGE, &location)?;
            self.charge_at(addon.as_str().len(), &location)?;
            self.charge_at(source.len(), &location)?;
        }
        if new_location {
            self.charge_at(OCCURRENCE_CHARGE, &location)?;
            self.charge_at(location.origin().addon().as_str().len(), &location)?;
            self.charge_at(location.origin().path().as_str().len(), &location)?;
        }
        let replacement = match self.catalogue {
            Some(catalogue) => {
                let translated = catalogue.lookup(addon, source);
                if translated == source {
                    None
                } else {
                    // Every occurrence is charged before allocating its expanded literal.
                    self.charge_at(translated.len(), &location)?;
                    Some(translated)
                }
            }
            None => None,
        };
        if let Some(locations) = self
            .terms
            .get_mut(addon)
            .and_then(|terms| terms.get_mut(source))
        {
            locations.insert(location);
        } else {
            // New keys/metadata and this occurrence were admitted above.
            self.terms
                .entry(addon.clone())
                .or_default()
                .insert(source.into(), BTreeSet::from([location]));
            self.term_count = next_count;
        }
        Ok(replacement.map(str::to_owned))
    }

    /// Clone only the new APIs' tree, preserving source-mode parsing, analysis and output.
    pub(crate) fn transform<'s>(
        &mut self,
        original: &XElement<'s>,
        loc: &LocCtx<'_>,
        provenance: Option<&Mapping>,
    ) -> Result<XElement<'s>, Diagnostic> {
        let mut root = original.clone();
        self.walk(&mut root, true, None, loc, provenance)?;
        Ok(root)
    }

    fn location(
        loc: &LocCtx<'_>,
        provenance: Option<&Mapping>,
        owner_offset: u32,
        literal_offset: u32,
    ) -> Result<Location, Diagnostic> {
        match provenance {
            Some(mapping) => mapping.translation_attribute_location(owner_offset),
            None => Ok(loc.at(literal_offset)),
        }
    }

    fn diagnostic(
        loc: &LocCtx<'_>,
        provenance: Option<&Mapping>,
        offset: u32,
        diagnostic: Diagnostic,
    ) -> Diagnostic {
        match provenance {
            Some(mapping) => mapping.remap(diagnostic.with_source_offset(offset)),
            None => diagnostic.with_location(loc.at(offset)),
        }
    }

    fn walk(
        &mut self,
        node: &mut XElement<'_>,
        enabled: bool,
        context: Option<&AddonName>,
        loc: &LocCtx<'_>,
        provenance: Option<&Mapping>,
    ) -> Result<(), Diagnostic> {
        let enabled = enabled && node.attr("t-translation") != Some("off");
        let own_context = match node.attr_node("t-translation-context") {
            Some(attr) => Some(AddonName::new(&attr.value).map_err(|_| {
                Self::diagnostic(
                    loc,
                    provenance,
                    attr.offset,
                    Diagnostic::error(
                        DiagnosticKind::InvalidDirectiveValue,
                        format!(
                            "t-translation-context=\"{}\": expected an addon name",
                            attr.value
                        ),
                    )
                    .with_directive("t-translation-context"),
                )
            })?),
            None => None,
        };
        let context = own_context.as_ref().or(context);
        if !self.validation_only && node.has("t-call") && (context.is_some() || !enabled) {
            return Err(Self::diagnostic(
                loc,
                provenance,
                node.offset,
                Diagnostic::error(DiagnosticKind::NotSupportedUntil, "caller translation context/off propagation into t-call is not supported until a later release").with_directive("translation"),
            ));
        }
        let component = node
            .name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_uppercase)
            || node.has("t-component");
        let call = node.has("t-call");
        let dom = !component
            && !call
            && node.name != "t"
            && !node.has("t-call-slot")
            && !node.has("t-slot");
        let mut attribute_contexts = BTreeMap::new();
        if dom {
            for attr in &node.attrs {
                if let Some(target) = attr.name.strip_prefix("t-translation-context-") {
                    if !translatable_attribute(target) || node.attr(target).is_none() {
                        return Err(Self::diagnostic(
                            loc,
                            provenance,
                            attr.offset,
                            Diagnostic::error(
                                DiagnosticKind::InvalidDirectiveValue,
                                format!(
                                    "{} requires a static translatable DOM attribute",
                                    attr.name
                                ),
                            )
                            .with_directive("t-translation-context"),
                        ));
                    }
                    let owner = AddonName::new(&attr.value).map_err(|_| {
                        Self::diagnostic(
                            loc,
                            provenance,
                            attr.offset,
                            Diagnostic::error(
                                DiagnosticKind::InvalidDirectiveValue,
                                format!("{}=\"{}\": expected an addon name", attr.name, attr.value),
                            )
                            .with_directive("t-translation-context"),
                        )
                    })?;
                    attribute_contexts.insert(target.to_owned(), owner);
                }
            }
        }
        for attr in &mut node.attrs {
            let selected = !self.validation_only
                && enabled
                && ((dom && translatable_attribute(attr.name))
                    || ((component || call)
                        && !attr.name.starts_with("t-")
                        && attr.name.ends_with(".translate")));
            if selected {
                let location = Self::location(loc, provenance, node.offset, attr.offset)?;
                let addon = attribute_contexts
                    .get(attr.name)
                    .or(context)
                    .unwrap_or(location.origin().addon());
                if let Some(value) = self.literal(addon, &attr.value, location.clone())? {
                    attr.value = Cow::Owned(value);
                }
            }
        }
        if dom {
            // These validated context declarations have been consumed by this pass. Source-mode
            // compilation retains them and its existing explicit unsupported diagnostic.
            node.attrs
                .retain(|attr| !attr.name.starts_with("t-translation-context-"));
        }
        for child in &mut node.children {
            match child {
                XNode::Element(element) => self.walk(element, enabled, context, loc, provenance)?,
                XNode::Text { text, offset } if enabled && !self.validation_only => {
                    let source = text.trim_matches(is_xml_space);
                    if source.is_empty() {
                        continue;
                    }
                    let location = match provenance {
                        Some(mapping) => mapping.translation_text_location(node.offset)?,
                        None => loc.at(*offset),
                    };
                    let addon = context.unwrap_or(location.origin().addon());
                    if let Some(value) = self.literal(addon, source, location.clone())? {
                        let start = text.len() - text.trim_start_matches(is_xml_space).len();
                        let end = text.trim_end_matches(is_xml_space).len();
                        let tail = text.len() - end;
                        self.charge_at(start, &location)?;
                        self.charge_at(tail, &location)?;
                        let Some(capacity) = start
                            .checked_add(value.len())
                            .and_then(|size| size.checked_add(tail))
                        else {
                            return Err(Diagnostic::over(
                                LimitKind::OutputBytes,
                                self.max_bytes,
                                usize::MAX,
                            )
                            .with_directive("translation")
                            .with_location(location.clone()));
                        };
                        let mut translated = String::with_capacity(capacity);
                        translated.push_str(&text[..start]);
                        translated.push_str(&value);
                        translated.push_str(&text[end..]);
                        *text = Cow::Owned(translated);
                    }
                }
                XNode::Text { .. } => {}
            }
        }
        Ok(())
    }
}

fn translatable_attribute(name: &str) -> bool {
    matches!(
        name,
        "alt"
            | "aria-label"
            | "aria-placeholder"
            | "aria-roledescription"
            | "aria-valuetext"
            | "label"
            | "placeholder"
            | "title"
            | "data-tooltip"
    )
}
