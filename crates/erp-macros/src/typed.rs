//! Typed table objects in `#[model]`/`#[extend]` (design): call
//! syntax inside the attribute that also takes the raw tuples, e.g.
//! `indexes = [index(name = "_open_by_date", fields = ["state", "start"], include = ["resource_id"])]`.
//!
//! Only the shape is checked here (known parameters, string literals, a known method); whether the
//! fields exist and suit the method is the registry's check when it builds the model.

use proc_macro2::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Expr, LitStr};

use crate::attrs::{array, err, lit_str};

/// The field-level `index = "<kind>"` rule for the kinds that only suit some field types (design
/// §6.1; the registry applies the same rule to declarations built at runtime): the text of the
/// compile error, or `None` when `field_kind` may have the index.
pub(crate) fn field_kind_mismatch(index_kind: &str, field_kind: &str) -> Option<String> {
    let (allowed, list): (&[&str], &str) = match index_kind {
        "brin" => (
            &["date", "datetime", "integer"],
            "date, datetime and integer",
        ),
        "gin" => (&["json"], "json"),
        // btree, btree_not_null and trigram suit any stored column; the caller has refused
        // unknown kinds already
        _ => return None,
    };
    if allowed.contains(&field_kind) {
        return None;
    }
    Some(format!(
        "index = \"{index_kind}\" is for {list} fields (this field is {field_kind})"
    ))
}

/// A typed table object written with call syntax in the attribute `key` (`indexes`), or `None`
/// when `expr` is not a call (a raw tuple, handled by the caller).
pub(crate) fn table_object(key: &str, expr: &Expr) -> syn::Result<Option<TokenStream>> {
    let Expr::Call(call) = expr else {
        return Ok(None);
    };
    let callee = match &*call.func {
        Expr::Path(p) => p.path.get_ident().map(ToString::to_string),
        _ => None,
    };
    match (key, callee.as_deref()) {
        ("indexes", Some("index")) => typed_index(call).map(Some),
        ("indexes", _) => err(
            call.func.span(),
            "expected `index(name = \"_...\", fields = [...], ...)` or a (\"_name\", \"definition\") tuple",
        ),
        _ => err(
            call.func.span(),
            format!(
                "`{key}` takes (\"_name\", ...) tuples; typed indexes go in `indexes = [index(...)]`"
            ),
        ),
    }
}

/// `index(name = "_x", fields = ["a", "b"], method = "btree", include = ["c"])`.
fn typed_index(call: &syn::ExprCall) -> syn::Result<TokenStream> {
    let rt = quote!(::erp_sdk::__private);
    let mut name: Option<LitStr> = None;
    let mut fields: Option<Vec<LitStr>> = None;
    let mut method: Option<TokenStream> = None;
    let mut include: Option<Vec<LitStr>> = None;
    let mut filter: Option<LitStr> = None;
    for arg in &call.args {
        let Expr::Assign(assign) = arg else {
            return err(arg.span(), "expected `parameter = value`");
        };
        let Expr::Path(key) = &*assign.left else {
            return err(assign.left.span(), "expected a parameter name");
        };
        let Some(key_name) = key.path.get_ident().map(ToString::to_string) else {
            return err(key.span(), "expected a parameter name");
        };
        let value = &*assign.right;
        let duplicate = match key_name.as_str() {
            "name" => name.replace(lit_str(value)?).is_some(),
            "fields" => fields.replace(names(value)?).is_some(),
            "include" => include.replace(names(value)?).is_some(),
            "method" => {
                let m = lit_str(value)?;
                let variant = match m.value().as_str() {
                    "btree" => quote!(Btree),
                    "brin" => quote!(Brin),
                    "gin" => quote!(Gin),
                    "gist" => quote!(Gist),
                    "trigram" => quote!(Trigram),
                    other => {
                        return err(
                            m.span(),
                            format!(
                                "unknown index method {other:?}: use \"btree\", \"brin\", \"gin\", \
                                 \"gist\" or \"trigram\""
                            ),
                        );
                    }
                };
                method.replace(quote!(#rt::IndexMethod::#variant)).is_some()
            }
            "filter" => filter.replace(lit_str(value)?).is_some(),
            other => {
                return err(
                    key.span(),
                    format!(
                        "unknown index parameter `{other}` (expected name, fields, method, include, filter)"
                    ),
                );
            }
        };
        if duplicate {
            return err(key.span(), format!("`{key_name}` is given twice"));
        }
    }
    let Some(name) = name else {
        return err(call.span(), "an index needs `name = \"_...\"`");
    };
    if !name.value().starts_with('_') {
        return err(
            name.span(),
            "names of SQL objects in a model start with '_' (e.g. \"_by_date\")",
        );
    }
    let Some(fields) = fields else {
        return err(call.span(), "an index needs `fields = [\"...\"]`");
    };
    if fields.is_empty() {
        return err(call.span(), "an index needs at least one field");
    }
    let method = method.unwrap_or_else(|| quote!(#rt::IndexMethod::Btree));
    let include = include.unwrap_or_default();
    let filter = filter.map(|f| quote!(.filter(#f)));
    Ok(quote!(#rt::TableObjectDecl::index(
        #name,
        #rt::IndexDecl::new(&[#(#fields),*]).method(#method).include(&[#(#include),*]) #filter
    )))
}

fn names(expr: &Expr) -> syn::Result<Vec<LitStr>> {
    array(expr)?.into_iter().map(lit_str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The macro keeps its own copy of the `brin`/`gin` type rule (a proc-macro crate cannot link
    /// the runtime crates); this pins it to `IndexKind::field_types`, the registry's source.
    #[test]
    fn the_macro_rule_is_the_registry_rule() {
        use erp_fields::IndexKind;
        let types = [
            "boolean",
            "integer",
            "float",
            "monetary",
            "char",
            "text",
            "html",
            "selection",
            "date",
            "datetime",
            "binary",
            "json",
            "many2one",
            "one2many",
            "many2many",
            "properties",
            "date_range",
            "datetime_range",
        ];
        for (name, kind) in [("brin", IndexKind::Brin), ("gin", IndexKind::Gin)] {
            for ty in types {
                let allowed = kind.field_types().is_none_or(|t| t.contains(&ty));
                let macro_says = field_kind_mismatch(name, ty);
                assert_eq!(macro_says.is_none(), allowed, "{name} on {ty}");
                if let (Some(text), Some(list)) = (macro_says, kind.field_types()) {
                    let expected = list.join(", ");
                    let expected = match expected.rsplit_once(", ") {
                        Some((init, last)) => format!("{init} and {last}"),
                        None => expected,
                    };
                    assert!(
                        text.contains(&format!("is for {expected} fields")),
                        "{text}"
                    );
                }
            }
        }
    }

    #[test]
    fn field_kind_rule() {
        assert_eq!(
            field_kind_mismatch("brin", "char").as_deref(),
            Some("index = \"brin\" is for date, datetime and integer fields (this field is char)")
        );
        assert_eq!(field_kind_mismatch("brin", "integer"), None);
        assert_eq!(
            field_kind_mismatch("gin", "date").as_deref(),
            Some("index = \"gin\" is for json fields (this field is date)")
        );
        assert_eq!(field_kind_mismatch("btree", "json"), None);
    }

    fn parse(src: &str) -> syn::Result<Option<TokenStream>> {
        let expr: Expr = syn::parse_str(src)?;
        table_object("indexes", &expr)
    }

    fn message(src: &str) -> String {
        match parse(src) {
            Ok(_) => panic!("{src} was accepted"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn typed_index_tokens() {
        let out = parse(
            "index(name = \"_open\", fields = [\"state\", \"start\"], include = [\"resource_id\"])",
        )
        .unwrap()
        .unwrap()
        .to_string();
        assert!(out.contains("TableObjectDecl :: index"), "{out}");
        assert!(out.contains("IndexMethod :: Btree"), "{out}");
        assert!(out.contains("\"resource_id\""), "{out}");
        let brin = parse("index(name = \"_d\", fields = [\"d\"], method = \"brin\")")
            .unwrap()
            .unwrap()
            .to_string();
        assert!(brin.contains("IndexMethod :: Brin"), "{brin}");
        let filtered =
            parse("index(name = \"_o\", fields = [\"s\"], filter = \"[('s', '=', 'open')]\")")
                .unwrap()
                .unwrap()
                .to_string();
        assert!(
            filtered.contains(". filter (\"[('s', '=', 'open')]\")"),
            "{filtered}"
        );
        // a raw tuple is the caller's
        assert!(parse("(\"_x\", \"(a)\")").unwrap().is_none());
    }

    #[test]
    fn typed_index_refusals() {
        assert_eq!(
            message("index(name = \"_x\", fields = [\"a\"], unique = true)"),
            "unknown index parameter `unique` (expected name, fields, method, include, filter)"
        );
        assert_eq!(
            message("index(name = \"_x\", fields = [\"a\"], filter = 1)"),
            "expected a string literal"
        );
        assert_eq!(
            message("index(name = \"_x\", fields = [\"a\"], filter = \"[]\", filter = \"[]\")"),
            "`filter` is given twice"
        );
        assert_eq!(
            message("index(name = \"_x\", fields = [\"a\"], method = \"hash\")"),
            "unknown index method \"hash\": use \"btree\", \"brin\", \"gin\", \"gist\" or \"trigram\""
        );
        assert_eq!(
            message("index(name = \"x\", fields = [\"a\"])"),
            "names of SQL objects in a model start with '_' (e.g. \"_by_date\")"
        );
        assert_eq!(
            message("index(name = \"_x\", fields = [])"),
            "an index needs at least one field"
        );
        assert_eq!(
            message("index(fields = [\"a\"])"),
            "an index needs `name = \"_...\"`"
        );
        assert_eq!(
            message("index(name = \"_x\", name = \"_y\", fields = [\"a\"])"),
            "`name` is given twice"
        );
        assert_eq!(
            message("key(name = \"_x\")"),
            "expected `index(name = \"_...\", fields = [...], ...)` or a (\"_name\", \"definition\") tuple"
        );
    }
}
