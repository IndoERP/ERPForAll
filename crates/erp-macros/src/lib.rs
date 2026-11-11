//! Proc macros for ERPForAll addons (layer L0).
//!
//! The output refers only to `::erp_sdk::__private`, so the same macros serve every build mode.
//!
//! - `#[model(name = "...", ...)]` on a struct declares a new model; the struct becomes an
//!   uninhabited declaration type (never a record) with typed field handles as associated consts.
//! - `#[extend(Model)]` on a struct extends an existing model in place (`_inherit`).
//! - Both take `row_version = true` and `keys = [key(name = "_x", fields = [...], message =
//!   "...")]`; each key also becomes an associated `KeyRef` constant (`_x` → `X`).
//! - `#[methods(Model)]` on an `impl` block registers its functions in the model's method chains (`#[method]` defines, `#[method(override)]` overrides and gets `sup: Super`)
//!   and generates a `<Type>Methods` trait for typed calls.
//! - `#[migration(addon = "...", version = "...", stage = pre | post | end)]` on a function
//!   registers an addon migration in `MIGRATIONS`.
//! - `addon!(name = "...", depends = [...])` declares the crate's addon.

use proc_macro::TokenStream;

mod attrs;
mod company_access;
mod descriptor;
mod excl;
mod keys;
mod methods;
mod migration;
mod model;
mod model_ref;
mod typed;

/// Declare a new model. See the crate documentation.
#[proc_macro_attribute]
pub fn model(attr: TokenStream, item: TokenStream) -> TokenStream {
    model::expand(attr.into(), item.into(), model::Mode::New)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Extend an existing model in place (`_inherit` with the same name).
#[proc_macro_attribute]
pub fn extend(attr: TokenStream, item: TokenStream) -> TokenStream {
    model::expand(attr.into(), item.into(), model::Mode::Extend)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Register the `#[method]` / `#[method(override)]` functions of an `impl` block for a model.
#[proc_macro_attribute]
pub fn methods(attr: TokenStream, item: TokenStream) -> TokenStream {
    methods::expand(attr.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Register an addon migration: `#[migration(addon = "...", version = "...", stage = pre | post |
/// end, name = "...")]` on an `async fn(&mut Transaction, &SemverVersion) -> Result<()>` or on a
/// `fn` of the `MigrationFn` type. `name` (default: the function's name) orders it
/// among the steps of the same version and stage.
#[proc_macro_attribute]
pub fn migration(attr: TokenStream, item: TokenStream) -> TokenStream {
    migration::expand(attr.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Declare the addon of this crate: `addon!(name = "sale_stock", depends = ["sale", "stock"])`.
#[proc_macro]
pub fn addon(input: TokenStream) -> TokenStream {
    model::expand_addon(input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Name a model that another addon declares (a data-only addon's `models/*.toml`, or a native
/// addon's `#[model]`), optionally with typed handles for its fields:
///
/// ```ignore
/// erp_sdk::model_ref!(
///     /// `sample_library.loan`, declared by the data-only addon `sample_library`.
///     pub Loan = "sample_library.loan" {
///         state: Option<Arc<str>>,
///         book_id: Many2one<Book>,
///         due_date: Option<Date>,
///     }
/// );
/// // Loan::STATE, Loan::BOOK_ID, Loan::DUE_DATE are `Field<Loan, T>` handles
/// ```
///
/// The type is uninhabited and carries the model's technical name; each listed field becomes an
/// associated `Field` const named after it in upper case. The field list is checked when the
/// addon is installed (`describe()` carries it): a field the model does not have, or one of a
/// kind that cannot hold the Rust type, refuses the install by name.
#[proc_macro]
pub fn model_ref(input: TokenStream) -> TokenStream {
    model_ref::expand(input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
