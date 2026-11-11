//! `keys = [key(name = "_x", fields = [...], message = "...", nulls_distinct = false)]` in
//! `#[model]`/`#[extend]` (design note §9).

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{Expr, LitStr};

use crate::attrs::{array, err, lit_bool, lit_str};

/// The `name = value` arguments of a call-syntax object (`key(name = "...", ...)`), checked
/// against `allowed`; repeated names are refused.
pub(crate) fn call_args<'a>(
    e: &'a Expr,
    object: &str,
    allowed: &[&str],
) -> syn::Result<Vec<(String, &'a Expr)>> {
    let Expr::Call(call) = e else {
        return err(e.span(), format!("expected {object}(name = \"_...\", ...)"));
    };
    match &*call.func {
        Expr::Path(p) if p.path.is_ident(object) => {}
        other => return err(other.span(), format!("expected {object}(...)")),
    }
    let mut out: Vec<(String, &Expr)> = Vec::new();
    for arg in &call.args {
        let Expr::Assign(assign) = arg else {
            return err(arg.span(), "expected `parameter = value`");
        };
        let Expr::Path(left) = &*assign.left else {
            return err(assign.left.span(), "expected a parameter name");
        };
        let Some(key) = left.path.get_ident().map(ToString::to_string) else {
            return err(assign.left.span(), "expected a parameter name");
        };
        if !allowed.contains(&key.as_str()) {
            return err(
                assign.left.span(),
                format!(
                    "unknown {object} parameter `{key}` (expected one of: {})",
                    allowed.join(", ")
                ),
            );
        }
        if out.iter().any(|(k, _)| *k == key) {
            return err(assign.left.span(), format!("`{key}` is given twice"));
        }
        out.push((key, &assign.right));
    }
    Ok(out)
}

/// Whether a `key` names `without_overlaps` (a temporal key, parsed in
/// `excl.rs`) rather than being a business key.
pub(crate) fn is_temporal(e: &Expr) -> bool {
    let Expr::Call(call) = e else { return false };
    call.args.iter().any(|a| {
        matches!(a, Expr::Assign(assign)
            if matches!(&*assign.left, Expr::Path(p) if p.path.is_ident("without_overlaps")))
    })
}

/// One `key(...)`: the table object declaration, the declared name and the associated constant
/// (the name without its leading `_`, upper-cased: `_quant_key` → `QUANT_KEY`).
pub(crate) fn parse_key(e: &Expr) -> syn::Result<(TokenStream, LitStr, syn::Ident)> {
    let rt = quote!(::erp_sdk::__private);
    let args = call_args(e, "key", &["name", "fields", "message", "nulls_distinct"])?;
    let get = |k: &str| args.iter().find(|(n, _)| n == k).map(|(_, v)| *v);
    let Some(name) = get("name") else {
        return err(e.span(), "key(...) needs name = \"_...\"");
    };
    let name = lit_str(name)?;
    let Some(suffix) = name.value().strip_prefix('_').map(str::to_owned) else {
        return err(
            name.span(),
            "key names start with '_' (e.g. \"_code_uniq\")",
        );
    };
    // the associated constant is the upper-cased suffix, so it must be a valid identifier of its
    // own: a letter first, then letters, digits and '_' (review RS-S2: `_1x` and `__` panicked)
    if !suffix.starts_with(|c: char| c.is_ascii_alphabetic())
        || !suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return err(
            name.span(),
            "key names are '_' followed by an ASCII letter, then ASCII letters, digits and '_'",
        );
    }
    let Some(fields) = get("fields") else {
        return err(e.span(), "key(...) needs fields = [\"...\", ...]");
    };
    let fields: Vec<LitStr> = array(fields)?
        .into_iter()
        .map(lit_str)
        .collect::<syn::Result<_>>()?;
    if fields.is_empty() {
        return err(e.span(), "key(...) needs at least one field");
    }
    let Some(message) = get("message") else {
        return err(
            e.span(),
            "key(...) needs message = \"...\" (shown on a duplicate)",
        );
    };
    let message = lit_str(message)?;
    let nulls_distinct = match get("nulls_distinct") {
        Some(v) => lit_bool(v)?,
        None => false,
    };
    let ident = format_ident!("{}", suffix.to_uppercase(), span = name.span());
    let obj = quote!(#rt::TableObjectDecl {
        name: #name,
        kind: #rt::TableObjectKind::Key(
            #rt::KeyDecl::new(&[#(#fields),*], #message).nulls_distinct(#nulls_distinct)
        ),
    });
    Ok((obj, name, ident))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(name: &str) -> syn::Result<(TokenStream, LitStr, syn::Ident)> {
        let text = format!("key(name = \"{name}\", fields = [\"a\"], message = \"m\")");
        let expr: Expr = syn::parse_str(&text).expect("a call expression");
        parse_key(&expr)
    }

    #[test]
    fn key_names_make_a_constant() {
        let (_, _, ident) = parse("_quant_key").expect("valid");
        assert_eq!(ident.to_string(), "QUANT_KEY");
    }

    #[test]
    fn bad_key_names_are_compile_errors_not_panics() {
        for name in ["_1x", "__", "_", "x", "_a-b"] {
            let error = parse(name).expect_err(name);
            let message = error.to_string();
            assert!(
                message.starts_with("key names "),
                "{name}: unexpected message {message}"
            );
        }
        assert_eq!(
            parse("_1x").unwrap_err().to_string(),
            "key names are '_' followed by an ASCII letter, then ASCII letters, digits and '_'"
        );
        assert_eq!(
            parse("x").unwrap_err().to_string(),
            "key names start with '_' (e.g. \"_code_uniq\")"
        );
    }
}
