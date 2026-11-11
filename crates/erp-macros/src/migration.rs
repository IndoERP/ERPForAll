//! `#[migration(addon = "...", version = "...", stage = ...)]`.
//!
//! The function runs in the upgrade's transaction and receives the module's previously installed
//! version. Two forms are accepted:
//!
//! - `async fn f(tx: &mut Transaction, from: &SemverVersion) -> Result<()>`: the macro keeps the
//!   function and generates a `MigrationFn`-shaped shim that boxes its future;
//! - `fn f<'a>(tx: &'a mut Transaction, from: &'a SemverVersion) -> BoxFuture<'a, Result<()>>`:
//!   already a `MigrationFn`, registered as it is (the compiler checks the type).
//!
//! The output refers only to `::erp_sdk::__private` (the crate rule, `lib.rs`).

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{Error, FnArg, GenericParam, Ident, ItemFn, LitStr, Result, Token};

#[derive(Clone, Copy)]
enum StageArg {
    Pre,
    Post,
    End,
}

impl StageArg {
    fn parse(value: &str, span: Span) -> Result<Self> {
        match value {
            "pre" => Ok(Self::Pre),
            "post" => Ok(Self::Post),
            "end" => Ok(Self::End),
            _ => Err(Error::new(
                span,
                format!("migration: stage must be pre, post or end, not `{value}`"),
            )),
        }
    }

    fn variant(self) -> Ident {
        format_ident!(
            "{}",
            match self {
                Self::Pre => "Pre",
                Self::Post => "Post",
                Self::End => "End",
            }
        )
    }
}

struct MigrationArgs {
    addon: LitStr,
    version: LitStr,
    stage: StageArg,
    name: Option<LitStr>,
}

fn non_empty(key: &str, lit: LitStr) -> Result<LitStr> {
    if lit.value().trim().is_empty() {
        return Err(Error::new(
            lit.span(),
            format!("migration: `{key}` must not be empty"),
        ));
    }
    Ok(lit)
}

impl Parse for MigrationArgs {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut addon = None;
        let mut version = None;
        let mut stage = None;
        let mut name = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let key_str = key.to_string();
            let duplicate =
                || Error::new(key.span(), format!("migration: `{key_str}` is given twice"));
            match key_str.as_str() {
                "addon" => {
                    if addon.is_some() {
                        return Err(duplicate());
                    }
                    addon = Some(non_empty("addon", input.parse()?)?);
                }
                "version" => {
                    if version.is_some() {
                        return Err(duplicate());
                    }
                    version = Some(non_empty("version", input.parse()?)?);
                }
                "stage" => {
                    if stage.is_some() {
                        return Err(duplicate());
                    }
                    // `stage = post` or `stage = "post"`
                    stage = Some(if input.peek(LitStr) {
                        let s: LitStr = input.parse()?;
                        StageArg::parse(&s.value(), s.span())?
                    } else {
                        let id: Ident = input.parse()?;
                        StageArg::parse(&id.to_string(), id.span())?
                    });
                }
                "name" => {
                    if name.is_some() {
                        return Err(duplicate());
                    }
                    name = Some(non_empty("name", input.parse()?)?);
                }
                _ => {
                    return Err(Error::new(
                        key.span(),
                        format!(
                            "migration: unknown argument `{key_str}`; expected addon, version, stage or name"
                        ),
                    ));
                }
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        let missing = |key: &str, example: &str| {
            Error::new(
                Span::call_site(),
                format!("migration: missing `{key} = {example}`"),
            )
        };
        Ok(Self {
            addon: addon.ok_or_else(|| missing("addon", "\"...\""))?,
            version: version.ok_or_else(|| missing("version", "\"...\""))?,
            stage: stage.ok_or_else(|| missing("stage", "pre | post | end"))?,
            name,
        })
    }
}

/// Refuse what cannot be a migration function, on the offending tokens.
fn check_signature(item: &ItemFn) -> Result<()> {
    let sig = &item.sig;
    if let Some(receiver) = sig.inputs.iter().find_map(|arg| match arg {
        FnArg::Receiver(r) => Some(r),
        FnArg::Typed(_) => None,
    }) {
        return Err(Error::new(
            receiver.span(),
            "migration: a migration is a free function, not a method",
        ));
    }
    if let Some(param) = sig
        .generics
        .params
        .iter()
        .find(|p| !matches!(p, GenericParam::Lifetime(_)))
    {
        return Err(Error::new(
            param.span(),
            "migration: a migration function cannot have type or const parameters",
        ));
    }
    if let Some(unsafety) = &sig.unsafety {
        return Err(Error::new(
            unsafety.span(),
            "migration: a migration function cannot be unsafe",
        ));
    }
    if let Some(abi) = &sig.abi {
        return Err(Error::new(
            abi.span(),
            "migration: a migration function cannot have an extern ABI",
        ));
    }
    if let Some(variadic) = &sig.variadic {
        return Err(Error::new(
            variadic.span(),
            "migration: a migration function cannot be variadic",
        ));
    }
    if sig.asyncness.is_some() && sig.inputs.len() != 2 {
        return Err(Error::new(
            sig.inputs.span(),
            format!(
                "migration: an async migration takes (tx: &mut Transaction, from: &SemverVersion), \
                 not {} argument(s)",
                sig.inputs.len()
            ),
        ));
    }
    Ok(())
}

pub fn expand(attr: TokenStream, item: TokenStream) -> Result<TokenStream> {
    let args: MigrationArgs = syn::parse2(attr)?;
    let item_fn: ItemFn = syn::parse2(item)?;
    check_signature(&item_fn)?;
    let rt = quote!(::erp_sdk::__private);
    let fn_ident = &item_fn.sig.ident;
    let name = match &args.name {
        Some(name) => name.clone(),
        None => LitStr::new(&fn_ident.to_string(), fn_ident.span()),
    };
    let addon = &args.addon;
    let version = &args.version;
    let stage = args.stage.variant();
    let static_ident = format_ident!("__ERP_MIGRATION_{}", fn_ident);

    // the function registered: the item itself, or a shim boxing the async item's future
    let (shim, entry) = if item_fn.sig.asyncness.is_some() {
        let shim_ident = format_ident!("__erp_migration_shim_{}", fn_ident);
        // spanned on the signature: a wrong argument or return type is reported there
        let shim = quote_spanned! {item_fn.sig.span()=>
            #[doc(hidden)]
            #[allow(non_snake_case)]
            fn #shim_ident<'__erp_a>(
                tx: &'__erp_a mut #rt::Transaction,
                from: &'__erp_a #rt::SemverVersion,
            ) -> #rt::BoxFuture<'__erp_a, #rt::Result<()>> {
                ::std::boxed::Box::pin(#fn_ident(tx, from))
            }
        };
        (shim, quote!(#shim_ident))
    } else {
        (TokenStream::new(), quote!(#fn_ident))
    };

    Ok(quote! {
        #item_fn

        #shim

        #[allow(non_upper_case_globals)]
        #[#rt::linkme::distributed_slice(#rt::MIGRATIONS)]
        #[linkme(crate = #rt::linkme)]
        static #static_ident: #rt::MigrationImpl = #rt::MigrationImpl::new(
            #addon,
            #version,
            #rt::MigrationStage::#stage,
            #name,
            #entry,
        ).at(::core::file!(), ::core::line!());
    })
}
