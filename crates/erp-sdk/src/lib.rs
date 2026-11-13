//! The ERPForAll addon API (layer L10) in two backends behind one surface.
//!
//! - **Native** (every target except WebAssembly): the ORM, registry and database crates; the addon
//!   runs inside the server process.
//! - **Guest** (`target_family = "wasm"`, i.e. `wasm32-wasip2`): a small facade over `erp-fields` and
//!   the guest bindings `erp-guest`. It pulls in no ORM, registry or database crate, and the addon
//!   runs as a sandboxed component.
//!
//! The same addon source compiles in both. The macros register a backend-neutral description of
//! every model, extension, method, override and compute; [`describe`] returns it as JSON, and a
//! component exports the same text through `describe()`.
//!
//! ```
//! erp_sdk::addon!(name = "test_inherit_base", depends = ["base"]);
//!
//! use erp_sdk::prelude::*;
//!
//! #[model(name = "test.inherit.mother", description = "Test Inherit Mother")]
//! pub struct Mother {
//!     #[field(char, default = "Foo")]
//!     pub name: Option<Arc<str>>,
//! }
//!
//! #[methods(Mother)]
//! impl Mother {
//!     #[method]
//!     async fn greet(this: &RecordSet<Mother>, tx: &mut Transaction) -> Result<String> {
//!         let name = this.ensure_one()?.get(tx, Mother::NAME).await?;
//!         Ok(format!("hello {}", name.as_deref().unwrap_or_default()))
//!     }
//! }
//!
//! fn main() {}
//! ```
//!
//! Declaration types (`Mother` above) are uninhabited: they name a model and carry its field
//! handles, never record data. Records are `RecordSet<Mother>` values.

#![allow(missing_docs)]

/// Validated HTML policies for field declarations; never a browser trust capability.
pub use erp_fields::html::{
    DefaultPolicy as HtmlDefaultPolicy, FieldPolicy as HtmlPolicy,
    SanitizePolicy as HtmlSanitizePolicy,
};

mod descriptor;

pub use descriptor::{DESCRIBE_SCHEMA, describe};

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(not(target_family = "wasm"))]
pub use native::*;

#[cfg(target_family = "wasm")]
mod guest;
#[cfg(target_family = "wasm")]
pub use guest::*;

/// Export the `addon` component world from the leaf addon crate, so a WASM build exposes
/// `describe()`. Write it once, in the crate that is compiled to the component (not in the addon
/// crates it depends on):
///
/// ```ignore
/// erp_sdk::export_component!();
/// ```
///
/// On a native build it expands to nothing, so the same source compiles in both modes. On
/// WebAssembly it needs `unsafe_code = "deny"` rather than the workspace's `forbid` in the crate's
/// `[lints.rust]`: the component ABI's export glue is unsafe by nature and is allowed on this one
/// expansion only. `invoke-method` runs the methods the addon registered; `compute-fields`,
/// `validate-constraints` and `onchange` return `unsupported` errors naming until a
/// follow-up wires them.
// `crate::__ERP_ADDON` must name the crate that writes the macro (the leaf addon), not this one
#[allow(clippy::crate_in_macro_def)]
#[macro_export]
macro_rules! export_component {
    () => {
        #[cfg(target_family = "wasm")]
        #[allow(unsafe_code)]
        const _: () = {
            struct __ErpSource;
            impl $crate::__private::GuestSource for __ErpSource {
                fn describe() -> ::std::string::String {
                    $crate::describe()
                }

                fn method(
                    model: &str,
                    method: &str,
                ) -> ::core::result::Result<
                    ::core::option::Option<$crate::__private::GuestMethodFn>,
                    $crate::__private::BoundaryError,
                > {
                    // a component runs the methods of the addon it is built from; the addons it
                    // was linked with keep their own layers
                    $crate::__private::find_guest_method(crate::ADDON.name, model, method)
                }
            }
            type __ErpExport = $crate::__private::Dispatching<__ErpSource>;
            $crate::__private::export_addon!(
                __ErpExport with_types_in $crate::__private::bindings
            );
        };
    };
}

/// Register one [`Contribution`](crate::__private::Contribution) with the backend's collector
/// (`linkme` natively, `inventory` on WebAssembly). Macro output only.
#[doc(hidden)]
#[macro_export]
macro_rules! __register_contribution {
    ($value:expr) => {
        #[cfg(not(target_family = "wasm"))]
        const _: () = {
            #[$crate::__private::linkme::distributed_slice($crate::__private::DESCRIPTORS)]
            #[linkme(crate = $crate::__private::linkme)]
            static __ERP_CONTRIBUTION: $crate::__private::Contribution = $value;
        };
        #[cfg(target_family = "wasm")]
        $crate::__private::inventory::submit! { $value }
    };
}
