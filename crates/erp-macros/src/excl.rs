//! `#[model]`/`#[extend]` arguments: `exclusions = [exclude(...)]`,
//! `keys = [key(..., without_overlaps = "...")]`, `extensions = [...]`, and the `bounds` of range
//! fields (design §5, §9).

use proc_macro2::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Expr, LitStr};

use crate::attrs::{array, err, lit_str, tuple};

fn rt() -> TokenStream {
    quote!(::erp_sdk::__private)
}

/// `"[)"` / `"[]"` as `RangeBounds` tokens.
pub(crate) fn bounds_tokens(text: &LitStr) -> syn::Result<TokenStream> {
    let rt = rt();
    match text.value().as_str() {
        "[)" => Ok(quote!(#rt::RangeBounds::HalfOpen)),
        "[]" => Ok(quote!(#rt::RangeBounds::Closed)),
        other => err(
            text.span(),
            format!("unknown bounds {other:?}: use \"[)\" or \"[]\""),
        ),
    }
}

/// The `name = value` arguments of a call such as `exclude(name = "_x", ...)`.
fn call_args<'a>(expr: &'a Expr, function: &str) -> syn::Result<Vec<(String, &'a Expr)>> {
    let Expr::Call(call) = expr else {
        return err(expr.span(), format!("expected {function}(...)"));
    };
    let is_function = matches!(&*call.func, Expr::Path(p) if p.path.is_ident(function));
    if !is_function {
        return err(call.func.span(), format!("expected {function}(...)"));
    }
    let mut out: Vec<(String, &Expr)> = Vec::new();
    for arg in &call.args {
        let Expr::Assign(assign) = arg else {
            return err(arg.span(), "expected `name = value`");
        };
        let Expr::Path(key) = &*assign.left else {
            return err(assign.left.span(), "expected a parameter name");
        };
        let Some(key) = key.path.get_ident().map(ToString::to_string) else {
            return err(assign.left.span(), "expected a parameter name");
        };
        if out.iter().any(|(k, _)| *k == key) {
            return err(assign.left.span(), format!("`{key}` is given twice"));
        }
        out.push((key, &assign.right));
    }
    Ok(out)
}

fn strings(expr: &Expr) -> syn::Result<Vec<LitStr>> {
    array(expr)?.into_iter().map(lit_str).collect()
}

/// `exclusions = [exclude(name = "_x", equal = [...], period = ("from", "to"), bounds = "[)",
/// filter = "<domain>", message = "...")]` as table object declarations.
pub(crate) fn exclusions(value: &Expr) -> syn::Result<Vec<TokenStream>> {
    let rt = rt();
    let mut out = Vec::new();
    for item in array(value)? {
        let args = call_args(item, "exclude")?;
        let (mut name, mut equal, mut period, mut bounds, mut filter, mut message) =
            (None, Vec::new(), None, None, None, None);
        for (key, value) in args {
            match key.as_str() {
                "name" => name = Some(lit_str(value)?),
                "equal" => equal = strings(value)?,
                "period" => {
                    let parts = tuple(value);
                    let [from, to] = parts.as_slice() else {
                        return err(value.span(), "expected period = (\"from\", \"to\")");
                    };
                    period = Some((lit_str(from)?, lit_str(to)?));
                }
                "bounds" => bounds = Some(bounds_tokens(&lit_str(value)?)?),
                "filter" => filter = Some(lit_str(value)?),
                "message" => message = Some(lit_str(value)?),
                other => {
                    return err(
                        value.span(),
                        format!(
                            "unknown exclude parameter `{other}` (name, equal, period, bounds, \
                             filter, message)"
                        ),
                    );
                }
            }
        }
        let missing =
            |what: &str| syn::Error::new(item.span(), format!("exclude(...) needs {what}"));
        let name = name.ok_or_else(|| missing("name = \"_...\""))?;
        let (from, to) = period.ok_or_else(|| missing("period = (\"from\", \"to\")"))?;
        let bounds = bounds.ok_or_else(|| missing("bounds = \"[)\" or \"[]\" (no default)"))?;
        let message = message.ok_or_else(|| missing("message = \"...\""))?;
        let filter = filter.map(|f| quote!(.filter(#f)));
        out.push(quote!(#rt::TableObjectDecl {
            name: #name,
            kind: #rt::TableObjectKind::Exclusion(
                #rt::ExclusionDecl::new((#from, #to), #bounds, #message)
                    .equal(&[#(#equal),*])
                    #filter
            ),
        }));
    }
    Ok(out)
}

/// `keys = [key(name = "_x", fields = [...], without_overlaps = "<range field>", message =
/// "...")]` as table object declarations. Only keys with `without_overlaps` are built here
/// (the dispatcher in `model.rs` sends the others to `keys.rs`, the business keys of
/// ).
pub(crate) fn keys(items: Vec<&Expr>) -> syn::Result<Vec<TokenStream>> {
    let rt = rt();
    let mut out = Vec::new();
    for item in items {
        let args = call_args(item, "key")?;
        let (mut name, mut fields, mut range, mut message) = (None, Vec::new(), None, None);
        for (key, value) in args {
            match key.as_str() {
                "name" => name = Some(lit_str(value)?),
                "fields" => fields = strings(value)?,
                "without_overlaps" => range = Some(lit_str(value)?),
                "message" => message = Some(lit_str(value)?),
                other => {
                    return err(
                        value.span(),
                        format!(
                            "unknown key parameter `{other}` (name, fields, without_overlaps, \
                             message)"
                        ),
                    );
                }
            }
        }
        let missing = |what: &str| syn::Error::new(item.span(), format!("key(...) needs {what}"));
        let name = name.ok_or_else(|| missing("name = \"_...\""))?;
        let Some(range) = range else {
            return err(
                item.span(),
                "internal: a key without `without_overlaps` reached the temporal-key parser",
            );
        };
        let message = message.ok_or_else(|| missing("message = \"...\""))?;
        out.push(quote!(#rt::TableObjectDecl {
            name: #name,
            kind: #rt::TableObjectKind::TemporalKey(
                #rt::TemporalKeyDecl::new(#range, #message).fields(&[#(#fields),*])
            ),
        }));
    }
    Ok(out)
}

/// `extensions = ["btree_gist", ...]` as `PgExtension` tokens.
pub(crate) fn extensions(value: &Expr) -> syn::Result<Vec<TokenStream>> {
    let rt = rt();
    let mut out = Vec::new();
    for item in array(value)? {
        let name = lit_str(item)?;
        out.push(match name.value().as_str() {
            "btree_gist" => quote!(#rt::PgExtension::BtreeGist),
            "pg_trgm" => quote!(#rt::PgExtension::PgTrgm),
            "unaccent" => quote!(#rt::PgExtension::Unaccent),
            other => {
                return err(
                    name.span(),
                    format!(
                        "unknown extension {other:?}: a model may declare \"btree_gist\", \
                         \"pg_trgm\" or \"unaccent\""
                    ),
                );
            }
        });
    }
    Ok(out)
}
