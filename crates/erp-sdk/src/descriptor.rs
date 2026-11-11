//! The backend-neutral description of an addon's contributions.
//!
//! `#[model]`, `#[extend]`, `#[methods]` and `addon!` register one [`Contribution`] per model,
//! extension, method, override, compute, constraint and onchange. The registration is a `linkme`
//! slice on native targets and an `inventory` collection on WebAssembly (the guard
//! showed that mechanism holds there). Neither collector
//! promises an order, so [`describe`] sorts. The `detail` text of each entry is JSON the macro
//! wrote from the source at compile time, which is why the native and the component build of one
//! source describe the same thing.

/// Version of the [`describe`] document. A change to the shape of an entry or of a `detail`
/// object that a consumer could misread raises it.
pub const DESCRIBE_SCHEMA: u32 = 1;

/// What a [`Contribution`] declares. Macro output only.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContributionKind {
    /// `addon!`.
    Addon,
    /// `#[model]`.
    Model,
    /// `#[extend]`.
    Extend,
    /// `#[method]`.
    Method,
    /// `#[method(override)]`.
    Override,
    /// `#[compute]`.
    Compute,
    /// `#[compute(override)]`.
    ComputeOverride,
    /// `#[constrains]`.
    Constraint,
    /// `#[onchange]`.
    Onchange,
    /// `model_ref!` with fields: the typed handles an addon names for fields another addon
    /// declares. The installer checks them against the registry.
    ModelRef,
}

impl ContributionKind {
    /// The name used in the document.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Addon => "addon",
            Self::Model => "model",
            Self::Extend => "extend",
            Self::Method => "method",
            Self::Override => "override",
            Self::Compute => "compute",
            Self::ComputeOverride => "compute_override",
            Self::Constraint => "constraint",
            Self::Onchange => "onchange",
            Self::ModelRef => "model_ref",
        }
    }
}

/// One declaration of an addon, as the macros see it. Macro output only; use [`describe`].
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct Contribution {
    /// The declaring addon.
    pub addon: &'static str,
    /// What is declared.
    pub kind: ContributionKind,
    /// The model it belongs to (empty for the addon itself).
    pub model: &'static str,
    /// The declaration type or function name.
    pub name: &'static str,
    /// A JSON object with the kind-specific attributes, written by the macro.
    pub detail: &'static str,
    /// When set, the attributes are computed by this function (the Rust types of a typed model
    /// reference are only known to the compiler, not to the macro) and `detail` is empty.
    pub detail_fn: Option<fn() -> String>,
}

impl Contribution {
    fn detail_text(&self) -> String {
        self.detail_fn
            .map_or_else(|| self.detail.to_owned(), |f| f())
    }
}

/// The `detail` of a `model_ref!` entry: `{"fields": [{"name", "kinds", "comodel"}]}`.
/// Macro output only.
#[doc(hidden)]
pub fn __model_ref_detail(fields: &[(&str, &[&str], Option<&str>)]) -> String {
    let list: Vec<erp_fields::serde_json::Value> = fields
        .iter()
        .map(|(name, kinds, comodel)| {
            erp_fields::serde_json::json!({"name": name, "kinds": kinds, "comodel": comodel})
        })
        .collect();
    erp_fields::serde_json::json!({ "fields": list }).to_string()
}

/// Every contribution linked into a native program.
#[cfg(not(target_family = "wasm"))]
#[doc(hidden)]
#[linkme::distributed_slice]
pub static DESCRIPTORS: [Contribution];

#[cfg(target_family = "wasm")]
inventory::collect!(Contribution);

fn collected() -> Vec<&'static Contribution> {
    #[cfg(not(target_family = "wasm"))]
    {
        DESCRIPTORS.iter().collect()
    }
    #[cfg(target_family = "wasm")]
    {
        inventory::iter::<Contribution>.into_iter().collect()
    }
}

/// Append `text` as a JSON string literal.
fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The document for a set of contributions.
fn render(entries: Vec<&Contribution>) -> String {
    let mut entries: Vec<(&Contribution, String)> =
        entries.into_iter().map(|c| (c, c.detail_text())).collect();
    entries.sort_by(|(a, ad), (b, bd)| {
        (a.addon, a.model, a.kind, a.name, ad).cmp(&(b.addon, b.model, b.kind, b.name, bd))
    });
    let mut out = String::with_capacity(64 + entries.len() * 160);
    out.push_str(&format!(
        "{{\"schema\":{DESCRIBE_SCHEMA},\"contributions\":["
    ));
    for (i, (c, detail)) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"addon\":");
        push_json_string(&mut out, c.addon);
        out.push_str(",\"kind\":");
        push_json_string(&mut out, c.kind.as_str());
        out.push_str(",\"model\":");
        push_json_string(&mut out, c.model);
        out.push_str(",\"name\":");
        push_json_string(&mut out, c.name);
        out.push_str(",\"detail\":");
        out.push_str(&detail);
        out.push('}');
    }
    out.push_str("]}");
    out
}

/// Everything the linked addons declare, as compact JSON text (schema [`DESCRIBE_SCHEMA`]).
///
/// Entries are sorted by `(addon, model, kind, name, detail)`, so the text is the same for the
/// same set of addons on every backend and in every link order. A component's `describe()` export
/// returns exactly this text.
///
/// ```
/// // an addon that declares nothing describes an empty set (this doctest links no addon)
/// let text = erp_sdk::describe();
/// assert!(text.starts_with(r#"{"schema":1,"contributions":["#));
/// ```
pub fn describe() -> String {
    render(collected())
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use erp_fields::serde_json;

    const A: Contribution = Contribution {
        addon: "a",
        kind: ContributionKind::Model,
        model: "m.x",
        name: "X",
        detail: "{}",
        detail_fn: None,
    };
    const B: Contribution = Contribution {
        addon: "a",
        kind: ContributionKind::Method,
        model: "m.x",
        name: "go",
        detail: "{\"args\":[]}",
        detail_fn: None,
    };

    #[test]
    fn the_document_does_not_depend_on_collection_order() {
        assert_eq!(render(vec![&A, &B]), render(vec![&B, &A]));
    }

    #[test]
    fn the_document_is_json_with_escaped_names() {
        let tricky = Contribution {
            name: "q\"\\\n\u{1}é",
            ..A
        };
        let text = render(vec![&tricky, &B]);
        let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(doc["schema"], DESCRIBE_SCHEMA);
        let names: Vec<&str> = doc["contributions"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert!(
            names.contains(&tricky.name) && names.contains(&"go"),
            "{names:?}"
        );
    }
}
