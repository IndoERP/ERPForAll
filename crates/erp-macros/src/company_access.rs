//! `company_access = …` of `#[model]`/`#[extend]` (A1-D2; design
//! ): the syntax is checked here, the meaning when the registry
//! is built.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{Expr, LitStr};

use crate::attrs::{err, lit_str};
use crate::model::rt;

/// What every malformed `company_access` value is told.
pub(crate) const USAGE: &str = "company_access must be direct(field = \"…\", branch = \"exact\" | \
                                \"ancestor\", empty = \"allow\" | \"deny\"[, parent = \"…\" | escape = \"…\"]), \
                                parent(edge = \"…\") or unrestricted(reason = \"…\")";

/// The longest reason, in bytes (`erp_registry::MAX_UNRESTRICTED_REASON_BYTES`; a test ties them).
pub(crate) const MAX_REASON_BYTES: usize = 512;

/// The message for a missing reason, a blank one (empty or only whitespace) or one longer than
/// [`MAX_REASON_BYTES`] bytes: the registry's words up to the comma (a test ties them).
fn reason_message() -> String {
    format!(
        "company_access: unrestricted(…) needs a reason that is not blank and at most \
         {MAX_REASON_BYTES} bytes long"
    )
}

/// The `CompanyBranch` variant a `branch = "…"` spelling stands for (the spellings are those of
/// `CompanyBranch::parse`; a test ties the two).
pub(crate) fn branch_variant(spelling: &str) -> Option<&'static str> {
    match spelling {
        "exact" => Some("Exact"),
        "ancestor" => Some("Ancestor"),
        _ => None,
    }
}

/// The `EmptyCompany` variant an `empty = "…"` spelling stands for (the spellings are those of
/// `EmptyCompany::parse`; a test ties the two).
pub(crate) fn empty_variant(spelling: &str) -> Option<&'static str> {
    match spelling {
        "deny" => Some("Deny"),
        "allow" => Some("Allow"),
        _ => None,
    }
}

/// The three forms.
#[derive(Clone, Copy)]
enum Form {
    Direct,
    Parent,
    Unrestricted,
}

impl Form {
    fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Parent => "parent",
            Self::Unrestricted => "unrestricted",
        }
    }

    fn parameters(self) -> &'static [&'static str] {
        match self {
            Self::Direct => &["field", "branch", "empty", "parent", "escape"],
            Self::Parent => &["edge"],
            Self::Unrestricted => &["reason"],
        }
    }
}

/// A non-empty name (`field`, `edge`) as a string literal.
fn name(expr: &Expr, what: &str) -> syn::Result<LitStr> {
    let s = lit_str(expr)?;
    if s.value().is_empty() {
        return err(
            s.span(),
            format!("company_access: the {what} name is empty"),
        );
    }
    Ok(s)
}

/// `company_access = direct(…) | parent(…) | unrestricted(…)` as a `CompanyAccessDecl`
/// expression.
pub(crate) fn tokens(expr: &Expr) -> syn::Result<TokenStream> {
    let rt = rt();
    let Expr::Call(call) = expr else {
        return err(expr.span(), USAGE);
    };
    let form = match &*call.func {
        Expr::Path(p) => p.path.get_ident().map(ToString::to_string),
        _ => None,
    };
    let form = match form.as_deref() {
        Some("direct") => Form::Direct,
        Some("parent") => Form::Parent,
        Some("unrestricted") => Form::Unrestricted,
        // any other callee, or a path that is not a bare name
        _ => return err(call.func.span(), USAGE),
    };
    let (allowed, form_name) = (form.parameters(), form.name());
    let mut given: Vec<(String, &Expr)> = Vec::new();
    for arg in &call.args {
        let Expr::Assign(assign) = arg else {
            return err(
                arg.span(),
                format!("company_access: expected `key = value` inside {form_name}(…)"),
            );
        };
        let key = match &*assign.left {
            Expr::Path(p) => p.path.get_ident().map(ToString::to_string),
            _ => None,
        };
        let Some(key) = key else {
            return err(
                assign.left.span(),
                format!("company_access: expected a parameter name inside {form_name}(…)"),
            );
        };
        if !allowed.contains(&key.as_str()) {
            return err(
                assign.left.span(),
                format!("company_access: unknown parameter '{key}' in {form_name}(…)"),
            );
        }
        if given.iter().any(|(k, _)| *k == key) {
            return err(
                arg.span(),
                format!("company_access: parameter '{key}' is given twice"),
            );
        }
        given.push((key, &*assign.right));
    }
    let get = |key: &str| given.iter().find(|(k, _)| k == key).map(|(_, e)| *e);
    match form {
        Form::Direct => {
            let missing: Vec<&str> = ["field", "branch", "empty"]
                .into_iter()
                .filter(|k| get(k).is_none())
                .collect();
            let (Some(field), Some(branch), Some(empty)) =
                (get("field"), get("branch"), get("empty"))
            else {
                return err(
                    call.span(),
                    format!(
                        "company_access: direct(…) needs field, branch and empty (missing: {})",
                        missing.join(", ")
                    ),
                );
            };
            let field = name(field, "field")?;
            let branch = lit_str(branch)?;
            let Some(branch) = branch_variant(&branch.value())
                .map(|v| format_ident!("{}", v, span = branch.span()))
            else {
                return err(
                    branch.span(),
                    "company_access: branch must be \"exact\" or \"ancestor\"",
                );
            };
            let empty = lit_str(empty)?;
            let Some(empty) =
                empty_variant(&empty.value()).map(|v| format_ident!("{}", v, span = empty.span()))
            else {
                return err(
                    empty.span(),
                    "company_access: empty must be \"allow\" or \"deny\"",
                );
            };
            Ok(match (get("parent"), get("escape")) {
                (Some(_), Some(escape)) => {
                    return err(
                        escape.span(),
                        "company_access: escape cannot be combined with parent in direct(…)",
                    );
                }
                (None, Some(escape)) => {
                    let escape = name(escape, "escape")?;
                    quote!(#rt::CompanyAccessDecl::direct_escaping(
                        #field, #rt::CompanyBranch::#branch, #rt::EmptyCompany::#empty, #escape
                    ))
                }
                (Some(edge), None) => {
                    let edge = name(edge, "edge")?;
                    quote!(#rt::CompanyAccessDecl::direct_and_parent(
                        #field, #rt::CompanyBranch::#branch, #rt::EmptyCompany::#empty, #edge
                    ))
                }
                (None, None) => quote!(#rt::CompanyAccessDecl::direct(
                    #field, #rt::CompanyBranch::#branch, #rt::EmptyCompany::#empty
                )),
            })
        }
        Form::Parent => {
            let Some(edge) = get("edge") else {
                return err(
                    call.span(),
                    "company_access: parent(…) needs edge (missing: edge)",
                );
            };
            let edge = name(edge, "edge")?;
            Ok(quote!(#rt::CompanyAccessDecl::parent(#edge)))
        }
        Form::Unrestricted => {
            let Some(reason) = get("reason") else {
                return err(call.span(), reason_message());
            };
            let reason = lit_str(reason)?;
            let text = reason.value();
            // the registry's rule (`erp_registry`'s `refused_reason`): blank or too long
            if text.len() > MAX_REASON_BYTES || text.trim().is_empty() {
                return err(reason.span(), reason_message());
            }
            Ok(quote!(#rt::CompanyAccessDecl::unrestricted(#reason)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The macro's spellings are exactly the registry's, variant for variant, and its reason
    /// limit is the registry's.
    #[test]
    fn company_access_spellings_are_the_registry_spellings() {
        for name in erp_registry::CompanyBranch::NAMES {
            let parsed = erp_registry::CompanyBranch::parse(name).unwrap();
            assert_eq!(
                branch_variant(name),
                Some(format!("{parsed:?}").as_str()),
                "{name}"
            );
        }
        for name in erp_registry::EmptyCompany::NAMES {
            let parsed = erp_registry::EmptyCompany::parse(name).unwrap();
            assert_eq!(
                empty_variant(name),
                Some(format!("{parsed:?}").as_str()),
                "{name}"
            );
        }
        assert_eq!(branch_variant("Exact"), None);
        assert_eq!(empty_variant(""), None);
        assert_eq!(
            MAX_REASON_BYTES,
            erp_registry::MAX_UNRESTRICTED_REASON_BYTES
        );
        assert_eq!(
            reason_message(),
            "company_access: unrestricted(…) needs a reason that is not blank and at most 512 \
             bytes long"
        );
        // the registry refuses a blank reason with the same words
        let registry = erp_registry::ModelDef::new("shop", "shop.doc")
            .company_access(erp_registry::CompanyAccessDef::unrestricted(" "))
            .check_limits()
            .unwrap_err();
        assert_eq!(
            registry.message(),
            format!("model 'shop.doc': {}, but it is blank", reason_message())
        );
    }

    fn refused(source: &str) -> String {
        let expr: Expr = syn::parse_str(source).unwrap();
        tokens(&expr).unwrap_err().to_string()
    }

    fn emitted(source: &str) -> String {
        let expr: Expr = syn::parse_str(source).unwrap();
        tokens(&expr).unwrap().to_string()
    }

    /// Every §3.2 message, each for the shape that causes it.
    #[test]
    fn company_access_syntax_errors() {
        for source in [
            "\"direct\"",
            "direct",
            "owner(edge = \"doc_id\")",
            "erp::direct(field = \"company_id\")",
        ] {
            assert_eq!(refused(source), USAGE, "{source}");
        }
        assert_eq!(
            refused("direct(field = \"company_id\")"),
            "company_access: direct(…) needs field, branch and empty (missing: branch, empty)"
        );
        assert_eq!(
            refused("direct()"),
            "company_access: direct(…) needs field, branch and empty (missing: field, branch, \
             empty)"
        );
        assert_eq!(
            refused("parent()"),
            "company_access: parent(…) needs edge (missing: edge)"
        );
        assert_eq!(
            refused(
                "direct(field = \"company_id\", branch = \"exact\", empty = \"deny\", edge = \"x\")"
            ),
            "company_access: unknown parameter 'edge' in direct(…)"
        );
        assert_eq!(
            refused("parent(field = \"x\")"),
            "company_access: unknown parameter 'field' in parent(…)"
        );
        assert_eq!(
            refused("unrestricted(why = \"x\")"),
            "company_access: unknown parameter 'why' in unrestricted(…)"
        );
        assert_eq!(
            refused("parent(edge = \"a\", edge = \"b\")"),
            "company_access: parameter 'edge' is given twice"
        );
        assert_eq!(
            refused("parent(\"doc_id\")"),
            "company_access: expected `key = value` inside parent(…)"
        );
        assert_eq!(
            refused("direct(field = \"company_id\", branch = \"parent_of\", empty = \"deny\")"),
            "company_access: branch must be \"exact\" or \"ancestor\""
        );
        assert_eq!(
            refused("direct(field = \"company_id\", branch = \"exact\", empty = \"False\")"),
            "company_access: empty must be \"allow\" or \"deny\""
        );
        assert_eq!(
            refused("direct(field = \"\", branch = \"exact\", empty = \"deny\")"),
            "company_access: the field name is empty"
        );
        assert_eq!(
            refused("parent(edge = \"\")"),
            "company_access: the edge name is empty"
        );
        assert_eq!(
            refused(
                "direct(field = \"company_id\", branch = \"exact\", empty = \"deny\", parent = \
                 \"\")"
            ),
            "company_access: the edge name is empty"
        );
        assert_eq!(
            refused(
                "direct(field = \"company_id\", branch = \"exact\", empty = \"deny\", escape = \"\")"
            ),
            "company_access: the escape name is empty"
        );
        assert_eq!(
            refused(
                "direct(field = \"company_id\", branch = \"exact\", empty = \"deny\", escape = \
                 \"share\", escape = \"share\")"
            ),
            "company_access: parameter 'escape' is given twice"
        );
        assert_eq!(
            refused(
                "direct(field = \"company_id\", branch = \"exact\", empty = \"deny\", parent = \
                 \"doc_id\", escape = \"share\")"
            ),
            "company_access: escape cannot be combined with parent in direct(…)"
        );
        assert_eq!(
            refused("parent(edge = \"doc_id\", escape = \"share\")"),
            "company_access: unknown parameter 'escape' in parent(…)"
        );
        assert_eq!(refused("unrestricted()"), reason_message());
        assert_eq!(refused("unrestricted(reason = \"\")"), reason_message());
        // only whitespace is blank too (review M1)
        for blank in ["\" \"", "\"   \"", "\"\\t\\r\\n\"", "\"\\u{a0}\\u{3000}\""] {
            assert_eq!(
                refused(&format!("unrestricted(reason = {blank})")),
                reason_message(),
                "{blank}"
            );
        }
        assert_eq!(
            refused(&format!("unrestricted(reason = \"{}\")", "r".repeat(513))),
            reason_message()
        );
        // bytes, not characters
        assert_eq!(
            refused(&format!("unrestricted(reason = \"{}é\")", "r".repeat(511))),
            reason_message()
        );
        assert_eq!(
            refused("parent(edge = 3)"),
            lit_str(&syn::parse_str::<Expr>("3").unwrap())
                .unwrap_err()
                .to_string(),
            "a value that is not a string is the shared literal error"
        );
    }

    /// Each form emits its constructor.
    #[test]
    fn company_access_forms_emit_their_constructor() {
        let rt = rt().to_string();
        assert_eq!(
            emitted("direct(field = \"company_id\", branch = \"ancestor\", empty = \"allow\")"),
            format!(
                "{rt} :: CompanyAccessDecl :: direct (\"company_id\" , {rt} :: CompanyBranch :: \
                 Ancestor , {rt} :: EmptyCompany :: Allow)"
            )
        );
        assert_eq!(
            emitted(
                "direct(empty = \"deny\", parent = \"tax_id\", branch = \"exact\", field = \
                 \"company_ids\")"
            ),
            format!(
                "{rt} :: CompanyAccessDecl :: direct_and_parent (\"company_ids\" , {rt} :: \
                 CompanyBranch :: Exact , {rt} :: EmptyCompany :: Deny , \"tax_id\")"
            )
        );
        assert_eq!(
            emitted(
                "direct(escape = \"share\", field = \"company_id\", branch = \"ancestor\", empty \
                 = \"allow\")"
            ),
            format!(
                "{rt} :: CompanyAccessDecl :: direct_escaping (\"company_id\" , {rt} :: \
                 CompanyBranch :: Ancestor , {rt} :: EmptyCompany :: Allow , \"share\")"
            )
        );
        assert_eq!(
            emitted("parent(edge = \"doc_id\")"),
            format!("{rt} :: CompanyAccessDecl :: parent (\"doc_id\")")
        );
        assert_eq!(
            emitted("unrestricted(reason = \" x \")"),
            format!("{rt} :: CompanyAccessDecl :: unrestricted (\" x \")")
        );
        let reason = format!("{}é", "r".repeat(510));
        assert_eq!(
            emitted(&format!("unrestricted(reason = \"{reason}\")")),
            format!("{rt} :: CompanyAccessDecl :: unrestricted (\"{reason}\")")
        );
    }
}
