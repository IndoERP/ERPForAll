//! The backend-neutral descriptors the macros register next to the native registrations
//! (`erp_sdk::describe`).
//!
//! A descriptor is written from the source text alone, at macro expansion, so the native build and
//! the WASM build of one source cannot describe different things. Values are the author's source
//! text (`to_string` of the tokens), never resolved types.

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use serde_json::{Map, Value, json};
use syn::Meta;

/// Source text of any syntax node.
pub(crate) fn text(node: &impl ToTokens) -> String {
    node.to_token_stream().to_string()
}

/// `{key: source text}` for attribute arguments; a bare flag (`required`) is `true`.
pub(crate) fn metas_json<'a>(metas: impl IntoIterator<Item = &'a Meta>) -> Value {
    let mut map = Map::new();
    for meta in metas {
        let key = crate::attrs::key(meta);
        let value = match meta {
            Meta::Path(_) => Value::Bool(true),
            Meta::NameValue(nv) => Value::String(text(&nv.value)),
            Meta::List(list) => Value::String(list.tokens.to_string()),
        };
        map.insert(key, value);
    }
    Value::Object(map)
}

/// `["a", "b"]` for string literals.
pub(crate) fn lits_json(lits: &[syn::LitStr]) -> Value {
    Value::Array(lits.iter().map(|l| json!(l.value())).collect())
}

/// Register one contribution with the backend's collector. `kind` is a `ContributionKind` variant;
/// `model` an expression of type `&'static str`.
pub(crate) fn register(
    addon: &TokenStream,
    kind: &str,
    model: &TokenStream,
    name: &str,
    detail: &Value,
) -> TokenStream {
    let kind = quote::format_ident!("{}", kind);
    let detail = detail.to_string();
    quote! {
        ::erp_sdk::__register_contribution!(::erp_sdk::__private::Contribution {
            addon: #addon,
            kind: ::erp_sdk::__private::ContributionKind::#kind,
            model: #model,
            name: #name,
            detail: #detail,
            detail_fn: ::core::option::Option::None,
        });
    }
}

/// `compile_error!` emitted for WebAssembly builds only, for a native-only feature.
pub(crate) fn wasm_unsupported(span: proc_macro2::Span, what: &str) -> TokenStream {
    let message = format!(
        "{what} is not available in WASM addons yet; a later release adds it (build natively, or \
         remove it from the code compiled to a component)"
    );
    quote::quote_spanned! {span=>
        #[cfg(target_family = "wasm")]
        compile_error!(#message);
    }
}
