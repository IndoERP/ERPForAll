//! Sanitising policies (spec section 4.0).
//!
//! A policy is a closed choice. [`DefaultPolicy`] is the full tag, attribute, form,
//! conditional-comment, URL and CSS filtering of the field allow-lists; its builders can only make
//! it stricter, so relaxing it is not representable.

/// How a field's text is treated on write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldPolicy {
    /// Sanitise with the given policy before the value is stored.
    Sanitize(SanitizePolicy),
    /// Keep the text as it is and never treat it as trusted markup (`sanitize=False`).
    UnsanitisedText,
}

/// A sanitising policy. Later slices add reviewed named presets; match it exhaustively so a new
/// preset breaks every consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SanitizePolicy {
    /// The default allow-lists, optionally made stricter.
    Default(DefaultPolicy),
}

/// The serialisation format of sanitised output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum OutputMethod {
    /// HTML5: void elements such as `<br>` are not closed.
    #[default]
    Html,
    /// XHTML: void elements are self-closed, `<br/>`.
    Xml,
}

/// The default sanitising policy: every filter on, plus the optional strictness added by the
/// `with_*` builders. Fields are private; there is no way to switch a filter off.
///
/// A struct literal does not compile:
///
/// ```compile_fail,E0451
/// use erp_html::{DefaultPolicy, OutputMethod};
///
/// let _ = DefaultPolicy {
///     strip_classes: false,
///     sanitize_style: false,
///     strip_style: false,
///     output_method: OutputMethod::Html,
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefaultPolicy {
    strip_classes: bool,
    sanitize_style: bool,
    strip_style: bool,
    output_method: OutputMethod,
}

impl DefaultPolicy {
    /// The default policy with no extra strictness and HTML output.
    #[must_use]
    pub const fn new() -> DefaultPolicy {
        DefaultPolicy {
            strip_classes: false,
            sanitize_style: false,
            strip_style: false,
            output_method: OutputMethod::Html,
        }
    }

    /// Also remove every `class` attribute.
    #[must_use]
    pub const fn with_strip_classes(mut self) -> DefaultPolicy {
        self.strip_classes = true;
        self
    }

    /// Also parse inline styles and keep only whitelisted properties.
    #[must_use]
    pub const fn with_sanitize_style(mut self) -> DefaultPolicy {
        self.sanitize_style = true;
        self
    }

    /// Also remove every `style` attribute and `<style>` element.
    #[must_use]
    pub const fn with_strip_style(mut self) -> DefaultPolicy {
        self.strip_style = true;
        self
    }

    /// Serialise as XHTML instead of HTML5.
    #[must_use]
    pub const fn with_xml_output(mut self) -> DefaultPolicy {
        self.output_method = OutputMethod::Xml;
        self
    }

    /// Whether `class` attributes are removed.
    #[must_use]
    pub fn strip_classes(&self) -> bool {
        self.strip_classes
    }

    /// Whether inline styles are reduced to whitelisted properties.
    #[must_use]
    pub fn sanitize_style(&self) -> bool {
        self.sanitize_style
    }

    /// Whether `style` attributes and `<style>` elements are removed.
    #[must_use]
    pub fn strip_style(&self) -> bool {
        self.strip_style
    }

    /// The output serialisation.
    #[must_use]
    pub fn output_method(&self) -> OutputMethod {
        self.output_method
    }
}

impl Default for DefaultPolicy {
    fn default() -> DefaultPolicy {
        DefaultPolicy::new()
    }
}

/// What a sanitising call does with its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SanitizeMode {
    /// Clean the markup and return the result with the removal report.
    Clean,
    /// Clean as `Clean` does, then reject with `DisallowedContent` if anything was removed by
    /// policy. Strictness only adds rejection; the policy itself is unchanged.
    Strict,
}
