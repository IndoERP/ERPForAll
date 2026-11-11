//! Attribute-argument helpers.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Expr, ExprLit, Lit, LitStr, Meta, Token};

/// Parse `a, b = c, d(...)` attribute arguments.
pub(crate) fn parse_metas(tokens: TokenStream) -> syn::Result<Vec<Meta>> {
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    Ok(syn::parse::Parser::parse2(parser, tokens)?
        .into_iter()
        .collect())
}

/// The key of a meta item as a string.
pub(crate) fn key(meta: &Meta) -> String {
    meta.path()
        .get_ident()
        .map(ToString::to_string)
        .unwrap_or_default()
}

pub(crate) fn value(meta: &Meta) -> syn::Result<&Expr> {
    match meta {
        Meta::NameValue(nv) => Ok(&nv.value),
        other => Err(syn::Error::new(other.span(), "expected `key = value`")),
    }
}

pub(crate) fn lit_str(expr: &Expr) -> syn::Result<LitStr> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => Ok(s.clone()),
        other => Err(syn::Error::new(other.span(), "expected a string literal")),
    }
}

pub(crate) fn lit_bool(expr: &Expr) -> syn::Result<bool> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Bool(b), ..
        }) => Ok(b.value),
        other => Err(syn::Error::new(other.span(), "expected `true` or `false`")),
    }
}

pub(crate) fn lit_u32(expr: &Expr) -> syn::Result<u32> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Int(i), ..
        }) => i.base10_parse(),
        other => Err(syn::Error::new(other.span(), "expected an integer literal")),
    }
}

/// A flag (`required`) or `required = true/false`.
pub(crate) fn flag(meta: &Meta) -> syn::Result<bool> {
    match meta {
        Meta::Path(_) => Ok(true),
        Meta::NameValue(nv) => lit_bool(&nv.value),
        Meta::List(l) => Err(syn::Error::new(l.span(), "expected a flag or `= bool`")),
    }
}

/// The elements of an array expression `[a, b]`.
pub(crate) fn array(expr: &Expr) -> syn::Result<Vec<&Expr>> {
    match expr {
        Expr::Array(a) => Ok(a.elems.iter().collect()),
        other => Err(syn::Error::new(other.span(), "expected `[...]`")),
    }
}

/// The elements of a tuple `(a, b)`, a parenthesized single value `(a)`, or a bare value.
pub(crate) fn tuple(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Tuple(t) => t.elems.iter().collect(),
        Expr::Paren(p) => vec![&*p.expr],
        other => vec![other],
    }
}

/// `"text"` as an `Option<&str>` token: `Some("text")`.
pub(crate) fn some_str(s: &LitStr) -> TokenStream {
    quote!(::core::option::Option::Some(#s))
}

pub(crate) fn err<T>(span: Span, message: impl std::fmt::Display) -> syn::Result<T> {
    Err(syn::Error::new(span, message))
}
