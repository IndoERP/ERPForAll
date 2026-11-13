//! Static allow and deny tables, plain data copied from the `definitions` block of,
//! which records the reference environment's lists.
//!
//! Every table is a sorted, duplicate-free slice of lower-case ASCII names, looked up by binary
//! search; the lookups are case-sensitive, so the caller passes names already lower-cased (the HTML
//! parser does this for tag and attribute names). The test `tables_equal_golden_definitions` holds
//! each table equal to the golden file.

/// Attributes kept on an element when attribute filtering is on (`safe_attrs`).
#[rustfmt::skip]
pub(crate) static SAFE_ATTRS: &[&str] = &[
    "abbr",
    "accept",
    "accept-charset",
    "accesskey",
    "action",
    "align",
    "alt",
    "aria-activedescendant",
    "aria-atomic",
    "aria-autocomplete",
    "aria-braillelabel",
    "aria-brailleroledescription",
    "aria-busy",
    "aria-checked",
    "aria-colcount",
    "aria-colindex",
    "aria-colindextext",
    "aria-colspan",
    "aria-controls",
    "aria-current",
    "aria-describedby",
    "aria-description",
    "aria-details",
    "aria-disabled",
    "aria-dropeffect",
    "aria-errormessage",
    "aria-expanded",
    "aria-flowto",
    "aria-grabbed",
    "aria-haspopup",
    "aria-hidden",
    "aria-invalid",
    "aria-keyshortcuts",
    "aria-label",
    "aria-labelledby",
    "aria-level",
    "aria-live",
    "aria-modal",
    "aria-multiline",
    "aria-multiselectable",
    "aria-orientation",
    "aria-owns",
    "aria-placeholder",
    "aria-posinset",
    "aria-pressed",
    "aria-readonly",
    "aria-relevant",
    "aria-required",
    "aria-roledescription",
    "aria-rowcount",
    "aria-rowindex",
    "aria-rowindextext",
    "aria-rowspan",
    "aria-selected",
    "aria-setsize",
    "aria-sort",
    "aria-valuemax",
    "aria-valuemin",
    "aria-valuenow",
    "aria-valuetext",
    "axis",
    "border",
    "cellpadding",
    "cellspacing",
    "char",
    "charoff",
    "charset",
    "checked",
    "cite",
    "class",
    "clear",
    "color",
    "cols",
    "colspan",
    "compact",
    "coords",
    "data-ai-field",
    "data-ai-record-id",
    "data-attachment-id",
    "data-behavior-props",
    "data-bs-toggle",
    "data-class",
    "data-embedded",
    "data-embedded-editable",
    "data-embedded-props",
    "data-file-name",
    "data-format-mimetype",
    "data-gl-filter",
    "data-heading-link-id",
    "data-height",
    "data-icon",
    "data-id",
    "data-interval",
    "data-language-id",
    "data-last-history-commits",
    "data-member_id",
    "data-mimetype",
    "data-mimetype-before-conversion",
    "data-o-datetime",
    "data-o-mail-full-composer",
    "data-o-mail-quote",
    "data-o-mail-quote-node",
    "data-oe-aria-label",
    "data-oe-expression",
    "data-oe-field",
    "data-oe-id",
    "data-oe-model",
    "data-oe-nodeid",
    "data-oe-protected",
    "data-oe-role",
    "data-oe-transient-content",
    "data-oe-translation-source-sha",
    "data-oe-type",
    "data-oe-version",
    "data-original-id",
    "data-original-mimetype",
    "data-original-src",
    "data-prop-name",
    "data-publish",
    "data-quality",
    "data-res_id",
    "data-resize-width",
    "data-scale-x",
    "data-scale-y",
    "data-scroll-background-ratio",
    "data-shape",
    "data-shape-colors",
    "data-view-id",
    "data-width",
    "data-x",
    "data-y",
    "datetime",
    "dir",
    "disabled",
    "enctype",
    "for",
    "frame",
    "headers",
    "height",
    "href",
    "hreflang",
    "hspace",
    "id",
    "ismap",
    "label",
    "lang",
    "longdesc",
    "maxlength",
    "media",
    "method",
    "multiple",
    "name",
    "nohref",
    "noshade",
    "nowrap",
    "prompt",
    "readonly",
    "rel",
    "rev",
    "role",
    "rows",
    "rowspan",
    "rules",
    "scope",
    "selected",
    "shape",
    "size",
    "span",
    "src",
    "start",
    "style",
    "summary",
    "tabindex",
    "target",
    "title",
    "type",
    "usemap",
    "valign",
    "value",
    "vspace",
    "width",
];

/// Whether `name` (lower-case) is in [`SAFE_ATTRS`].
#[must_use]
pub(crate) fn is_safe_attr(name: &str) -> bool {
    SAFE_ATTRS.binary_search(&name).is_ok()
}

/// Attributes whose value is a URL and so is checked against the scheme allow-list (`link_attrs`).
#[rustfmt::skip]
pub(crate) static LINK_ATTRS: &[&str] = &[
    "action",
    "archive",
    "background",
    "cite",
    "classid",
    "codebase",
    "data",
    "dynsrc",
    "formaction",
    "href",
    "longdesc",
    "lowsrc",
    "profile",
    "src",
    "usemap",
    "xlink:href",
];

/// Whether `name` (lower-case) is in [`LINK_ATTRS`].
#[must_use]
pub(crate) fn is_link_attr(name: &str) -> bool {
    LINK_ATTRS.binary_search(&name).is_ok()
}

/// Elements kept by the tag allow-list (`allow_tags`); every other element is unwrapped.
#[rustfmt::skip]
pub(crate) static ALLOW_TAGS: &[&str] = &[
    "a",
    "abbr",
    "acronym",
    "address",
    "applet",
    "area",
    "article",
    "aside",
    "audio",
    "b",
    "base",
    "basefont",
    "bdi",
    "bdo",
    "big",
    "blink",
    "blockquote",
    "body",
    "br",
    "button",
    "canvas",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "command",
    "datalist",
    "dd",
    "del",
    "details",
    "dfn",
    "dir",
    "div",
    "dl",
    "dt",
    "em",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "font",
    "footer",
    "form",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hgroup",
    "hr",
    "html",
    "i",
    "iframe",
    "img",
    "input",
    "ins",
    "isindex",
    "kbd",
    "keygen",
    "label",
    "legend",
    "li",
    "link",
    "main",
    "map",
    "mark",
    "marquee",
    "math",
    "menu",
    "meta",
    "meter",
    "nav",
    "noscript",
    "object",
    "ol",
    "optgroup",
    "option",
    "output",
    "p",
    "param",
    "pre",
    "progress",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "script",
    "section",
    "select",
    "small",
    "source",
    "span",
    "strike",
    "strong",
    "style",
    "sub",
    "summary",
    "sup",
    "svg",
    "table",
    "tbody",
    "td",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "time",
    "title",
    "tr",
    "track",
    "tt",
    "u",
    "ul",
    "var",
    "video",
    "wbr",
];

/// Whether `name` (lower-case) is in [`ALLOW_TAGS`].
#[must_use]
pub(crate) fn is_allowed_tag(name: &str) -> bool {
    ALLOW_TAGS.binary_search(&name).is_ok()
}

/// Elements removed together with their whole subtree (`kill_tags`).
#[rustfmt::skip]
pub(crate) static KILL_TAGS: &[&str] = &[
    "base",
    "embed",
    "frame",
    "head",
    "iframe",
    "link",
    "meta",
    "noscript",
    "object",
    "script",
    "style",
    "title",
];

/// Whether `name` (lower-case) is in [`KILL_TAGS`].
#[must_use]
pub(crate) fn is_kill_tag(name: &str) -> bool {
    KILL_TAGS.binary_search(&name).is_ok()
}

/// Wrapper elements removed while their children are kept (`remove_tags`).
#[rustfmt::skip]
pub(crate) static REMOVE_TAGS: &[&str] = &[
    "body",
    "html",
];

/// Whether `name` (lower-case) is in [`REMOVE_TAGS`].
#[must_use]
pub(crate) fn is_remove_tag(name: &str) -> bool {
    REMOVE_TAGS.binary_search(&name).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_sorted_and_unique() {
        for (name, table) in [
            ("safe_attrs", SAFE_ATTRS),
            ("link_attrs", LINK_ATTRS),
            ("allow_tags", ALLOW_TAGS),
            ("kill_tags", KILL_TAGS),
            ("remove_tags", REMOVE_TAGS),
        ] {
            assert!(
                table.windows(2).all(|w| w[0] < w[1]),
                "{name} is strictly ascending"
            );
        }
    }

    #[test]
    fn lookups_find_members_and_refuse_others() {
        assert!(is_safe_attr("href") && is_link_attr("href"));
        assert!(is_safe_attr("aria-label"));
        assert!(!is_safe_attr("onclick"));
        assert!(
            !is_safe_attr("formaction"),
            "formaction is a link attribute, not a safe one"
        );
        assert!(is_link_attr("formaction"));
        assert!(is_allowed_tag("p"));
        assert!(!is_allowed_tag("xyz"));
        // the reference lists overlap: a kill tag can also be in the allow list (kill wins)
        assert!(is_allowed_tag("script") && is_kill_tag("script"));
        assert!(is_kill_tag("script"));
        assert!(!is_kill_tag("p"));
        assert!(is_remove_tag("body"));
        assert!(is_remove_tag("html"));
        assert!(!is_remove_tag("div"));
        // case-sensitive: callers pass lower-cased names
        assert!(!is_allowed_tag("P"));
        assert!(!is_allowed_tag(""));
    }
}
