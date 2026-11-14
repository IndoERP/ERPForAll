//! `#[methods(Model)]`: method chain entries and typed call traits.
//!
//! Accepted signatures (arguments must be owned, `'static` types):
//!
//! ```text
//! #[method]   async fn name(this: &RecordSet<M>, tx: &mut Transaction, a: A, b: B) -> Result<R>
//! #[method(override)] async fn name(this: &RecordSet<M>, tx: &mut Transaction, a: A, b: B, sup: Super) -> Result<R>
//! #[onchange("field", ...)] async fn name(this: &RecordSet<M>, tx: &mut Transaction) -> Result<OnchangeResult>
//! ```
//!
//! `#[field_access(context = ["key", ...])]` marks a synchronous override of the model's field access
//! (`erp_orm::field_chain`), no `this` and no `tx`:
//!
//! ```text
//! #[field_access(context = ["project_sharing_create"])]
//! fn name(ctx: &FieldAccessCtx<'_>, op: FieldOp, mask: FieldMask, sup: FieldAccessSuper) -> Result<FieldMask>
//! ```
//!
//! `Super` may be written without generics; the macro fills in `Super<(A, B), R>`.
//!
//! `#[rpc(...)]` next to `#[method]` exposes the method over RPC:
//!
//! `#[rpc]` without `effect` (and without `hide`), an unknown effect, a parameter named `ids` or
//! `context`, and a method name starting with `_` are compile errors. `assistant` takes only
//! `offer`, and never with `effect = external` or `hide`; it adds an `AssistantOffer` entry, and
//! every parameter carries its `FromRpc::SHAPE` for the tool schema.
//!
//! `#[data_callable]` (or `#[data_callable(model)]`) next to `#[method]` lets a `<function>` of a
//! data file call the method: same parameter table and binder as `#[rpc]`, no effect
//! class, not callable over RPC. It may sit on a `_private` method, and next to `#[rpc(hide)]`.
//! A method with `#[rpc(effect = …)]` is data-callable already; both on one method is an error.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{
    FnArg, GenericArgument, ImplItem, ItemImpl, Meta, Path, PathArguments, ReturnType, Type,
};

use crate::attrs::{err, key, lit_str, parse_metas, value};
use crate::model::addon_tokens;

struct Method {
    name: syn::Ident,
    is_override: bool,
    /// `#[method(wire)]`: the types can be serialised, so the method can sit in a chain with a
    /// WebAssembly implementation.
    wire: bool,
    /// `#[method(rpc)]`: a package of this addon exposes the method to RPC clients (carried in
    /// `describe()` as `"rpc": true`). Natively it is a no-op: `#[rpc]` is the native marker.
    package_rpc: bool,
    args: Vec<(syn::Pat, Type)>,
    ret: Type,
    line_span: Span,
    /// `#[constrains("a", "b")]`: the field names that trigger the constraint.
    constrains: Option<Vec<syn::LitStr>>,
    /// `#[onchange("a", "b")]`: the field names whose change runs the onchange method.
    onchange: Option<Vec<syn::LitStr>>,
    /// `#[rpc(...)]`.
    rpc: Option<Rpc>,
    /// `#[data_callable]` (an [`Rpc::Data`]).
    data: Option<Rpc>,
}

/// A parsed `#[rpc(...)]`.
enum Rpc {
    /// `#[rpc(effect = …[, model][, assistant = offer])]`, with each argument's
    /// `#[arg(default = …)]`.
    Expose {
        effect: syn::Ident,
        model: bool,
        assistant: bool,
        defaults: Vec<Option<syn::Expr>>,
    },
    /// `#[rpc(hide)]`.
    Hide,
    /// `#[data_callable[(model)]]`, with each argument's `#[arg(default = …)]`.
    Data {
        model: bool,
        defaults: Vec<Option<syn::Expr>>,
    },
}

/// Parse `#[data_callable]` / `#[data_callable(model)]`: whether it is a model method.
fn parse_data_callable(attr: &syn::Attribute) -> syn::Result<bool> {
    const EXPECTED: &str = "expected #[data_callable] or #[data_callable(model)]";
    match &attr.meta {
        Meta::Path(_) => Ok(false),
        Meta::List(l) => {
            let metas = parse_metas(l.tokens.clone())?;
            match metas.as_slice() {
                [Meta::Path(p)] if p.is_ident("model") => Ok(true),
                _ => err(attr.span(), EXPECTED),
            }
        }
        Meta::NameValue(nv) => err(nv.span(), EXPECTED),
    }
}

const EFFECTS: [(&str, &str); 4] = [
    ("read", "Read"),
    ("write", "Write"),
    ("destructive", "Destructive"),
    ("external", "External"),
];

/// A parsed `#[rpc(...)]` attribute, before the signature checks.
struct RpcAttr {
    /// The `Effect` variant, absent with `hide`.
    effect: Option<syn::Ident>,
    /// `model`.
    model: bool,
    /// `hide`.
    hide: bool,
    /// `assistant = offer` ((a)).
    assistant: bool,
}

/// Parse `#[rpc(effect = …, model, assistant = offer)]` / `#[rpc(hide)]`.
fn parse_rpc(attr: &syn::Attribute) -> syn::Result<RpcAttr> {
    const NEEDS_EFFECT: &str =
        "#[rpc] needs an effect: #[rpc(effect = read|write|destructive|external)] or #[rpc(hide)]";
    let metas = match &attr.meta {
        Meta::Path(p) => return err(p.span(), NEEDS_EFFECT),
        Meta::List(l) => parse_metas(l.tokens.clone())?,
        Meta::NameValue(nv) => return err(nv.span(), NEEDS_EFFECT),
    };
    let (mut effect, mut model, mut hide) = (None, false, false);
    let mut assistant: Option<Span> = None;
    for meta in &metas {
        match (key(meta).as_str(), meta) {
            ("assistant", Meta::NameValue(nv)) => {
                if assistant.is_some() {
                    return err(nv.span(), "`assistant` is given twice");
                }
                let is_offer = matches!(
                    &nv.value,
                    syn::Expr::Path(p) if p.path.is_ident("offer")
                );
                if !is_offer {
                    return err(nv.value.span(), "expected assistant = offer");
                }
                assistant = Some(nv.span());
            }
            ("effect", Meta::NameValue(nv)) => {
                if effect.is_some() {
                    return err(nv.span(), "`effect` is given twice");
                }
                let syn::Expr::Path(path) = &nv.value else {
                    return err(
                        nv.value.span(),
                        "expected effect = read|write|destructive|external",
                    );
                };
                let Some(ident) = path.path.get_ident() else {
                    return err(
                        path.span(),
                        "expected effect = read|write|destructive|external",
                    );
                };
                let Some((_, variant)) = EFFECTS.iter().find(|(name, _)| ident == name) else {
                    return err(
                        ident.span(),
                        format!(
                            "unknown effect `{ident}`: expected read, write, destructive or \
                             external"
                        ),
                    );
                };
                effect = Some(syn::Ident::new(variant, ident.span()));
            }
            ("model", Meta::Path(_)) if !model => model = true,
            ("hide", Meta::Path(_)) if !hide => hide = true,
            ("model" | "hide", Meta::Path(_)) => {
                return err(meta.span(), format!("`{}` is given twice", key(meta)));
            }
            (other, _) => {
                return err(
                    meta.span(),
                    format!(
                        "unknown #[rpc] parameter `{other}`: expected effect, model, assistant or \
                         hide"
                    ),
                );
            }
        }
    }
    if hide && (effect.is_some() || model || assistant.is_some()) {
        return err(attr.span(), "#[rpc(hide)] takes no other parameter");
    }
    if !hide && effect.is_none() {
        return err(attr.span(), NEEDS_EFFECT);
    }
    if let Some(span) = assistant
        && effect.as_ref().is_some_and(|e| e == "External")
    {
        return err(
            span,
            "assistant = offer cannot be given with effect = external: external methods are never \
             offered to the assistant",
        );
    }
    Ok(RpcAttr {
        effect,
        model,
        hide,
        assistant: assistant.is_some(),
    })
}

/// Take `#[arg(default = expr)]` off a parameter.
fn take_arg_default(t: &mut syn::PatType) -> syn::Result<Option<syn::Expr>> {
    let Some(pos) = t.attrs.iter().position(|a| a.path().is_ident("arg")) else {
        return Ok(None);
    };
    let attr = t.attrs.remove(pos);
    if t.attrs.iter().any(|a| a.path().is_ident("arg")) {
        return err(attr.span(), "one #[arg(...)] per parameter");
    }
    let metas = match &attr.meta {
        Meta::List(l) => parse_metas(l.tokens.clone())?,
        other => return err(other.span(), "expected #[arg(default = <expression>)]"),
    };
    match metas.as_slice() {
        [Meta::NameValue(nv)] if nv.path.is_ident("default") => Ok(Some(nv.value.clone())),
        _ => err(attr.span(), "expected #[arg(default = <expression>)]"),
    }
}

/// A `#[field_access(...)]` function.
struct FieldAccess {
    name: syn::Ident,
    context: Vec<syn::LitStr>,
    line_span: Span,
}

/// Parse `#[field_access(context = [...])]` and check the signature; rewrites the `sup` parameter
/// type.
fn parse_field_access(
    f: &mut syn::ImplItemFn,
    pos: usize,
    rt: &TokenStream,
) -> syn::Result<FieldAccess> {
    let attr = f.attrs.remove(pos);
    let metas = match &attr.meta {
        Meta::Path(_) => Vec::new(),
        Meta::List(l) => parse_metas(l.tokens.clone())?,
        Meta::NameValue(nv) => return err(nv.span(), "expected #[field_access(context = [...])]"),
    };
    let mut context = Vec::new();
    for meta in &metas {
        match key(meta).as_str() {
            "context" => context = str_list(meta)?,
            other => {
                return err(
                    meta.span(),
                    format!("unknown #[field_access] parameter `{other}`: expected context"),
                );
            }
        }
    }
    if f.sig.asyncness.is_some() {
        return err(
            f.sig.asyncness.span(),
            "a #[field_access] override is a synchronous `fn` (it sees no transaction)",
        );
    }
    if f.sig.inputs.len() != 4 {
        return err(
            f.sig.span(),
            "expected fn(ctx: &FieldAccessCtx<'_>, op: FieldOp, mask: FieldMask, sup: FieldAccessSuper) -> Result<FieldMask>",
        );
    }
    if let Some(FnArg::Typed(t)) = f.sig.inputs.last_mut() {
        let span = t.ty.span();
        *t.ty = syn::parse2(quote_spanned!(span=> #rt::FieldAccessSuper<'_>))?;
    }
    Ok(FieldAccess {
        name: f.sig.ident.clone(),
        context,
        line_span: f.sig.ident.span(),
    })
}

/// A `#[search_method]` function.
struct SearchMethod {
    name: syn::Ident,
    takes_super: bool,
    span: Span,
}

fn parse_search_method(
    f: &mut syn::ImplItemFn,
    pos: usize,
    _rt: &TokenStream,
) -> syn::Result<SearchMethod> {
    let attr = f.attrs.remove(pos);
    let mut is_override = false;
    match &attr.meta {
        Meta::Path(_) => {}
        Meta::List(l) => {
            let tokens = l.tokens.clone();
            let mut iter = tokens.into_iter();
            match iter.next() {
                Some(proc_macro2::TokenTree::Ident(i))
                    if i == "override" && iter.next().is_none() =>
                {
                    is_override = true;
                }
                _ => {
                    return err(
                        attr.span(),
                        "expected #[search_method] or #[search_method(override)]",
                    );
                }
            }
        }
        Meta::NameValue(nv) => {
            return err(
                nv.span(),
                "expected #[search_method] or #[search_method(override)]",
            );
        }
    }
    if f.sig.asyncness.is_some() {
        return err(
            f.sig.fn_token.span(),
            "#[search_method] is synchronous: fn(req: &FieldSearchRequest<'_>, ctx: &mut FieldSearchContext<'_>[, sup: FieldSearchSuper<'_>]) -> Result<Domain>",
        );
    }
    if f.sig.inputs.iter().any(|i| matches!(i, FnArg::Receiver(_))) {
        return err(f.sig.span(), "#[search_method] does not take &self or self");
    }
    let takes_super = match f.sig.inputs.len() {
        2 => {
            if is_override {
                return err(
                    f.sig.span(),
                    "#[search_method(override)] requires 3 parameters: fn(req: &FieldSearchRequest<'_>, ctx: &mut FieldSearchContext<'_>, sup: FieldSearchSuper<'_>) -> Result<Domain>",
                );
            }
            false
        }
        3 => true,
        _ => {
            return err(
                f.sig.span(),
                if is_override {
                    "expected fn(req: &FieldSearchRequest<'_>, ctx: &mut FieldSearchContext<'_>, sup: FieldSearchSuper<'_>) -> Result<Domain>"
                } else {
                    "expected fn(req: &FieldSearchRequest<'_>, ctx: &mut FieldSearchContext<'_>[, sup: FieldSearchSuper<'_>]) -> Result<Domain>"
                },
            );
        }
    };
    fn named_type(ty: &syn::Type, name: &str) -> bool {
        matches!(ty, syn::Type::Path(path) if path.path.segments.last().is_some_and(|s| s.ident == name))
    }
    for (index, name, mutable) in [
        (0, "FieldSearchRequest", false),
        (1, "FieldSearchContext", true),
    ] {
        let valid = match f.sig.inputs.iter().nth(index) {
            Some(FnArg::Typed(t)) => {
                matches!(&*t.ty, syn::Type::Reference(reference) if reference.mutability.is_some() == mutable && named_type(&reference.elem, name))
            }
            _ => false,
        };
        if !valid {
            return err(
                f.sig.span(),
                format!(
                    "#[search_method] parameter {} must be {}{name}<'_>",
                    index + 1,
                    if mutable { "&mut " } else { "&" }
                ),
            );
        }
    }
    let valid_result = match &f.sig.output {
        syn::ReturnType::Type(_, ty) => match &**ty {
            syn::Type::Path(path) => path.path.segments.last().is_some_and(|segment| {
                if segment.ident != "Result" { return false; }
                match &segment.arguments {
                    syn::PathArguments::AngleBracketed(args) => args.args.len() == 1 && matches!(args.args.first(), Some(syn::GenericArgument::Type(ty)) if named_type(ty, "Domain")),
                    _ => false,
                }
            }),
            _ => false,
        },
        syn::ReturnType::Default => false,
    };
    if !valid_result {
        return err(
            f.sig.output.span(),
            "#[search_method] must return Result<Domain>",
        );
    }
    if takes_super && let Some(FnArg::Typed(t)) = f.sig.inputs.last() {
        let valid = match &*t.ty {
            syn::Type::Path(path) => path
                .path
                .segments
                .last()
                .is_some_and(|s| s.ident == "FieldSearchSuper"),
            _ => false,
        };
        if !valid {
            return err(
                t.ty.span(),
                "#[search_method] third parameter must be FieldSearchSuper<'_>",
            );
        }
    }
    Ok(SearchMethod {
        name: f.sig.ident.clone(),
        takes_super,
        span: f.sig.ident.span(),
    })
}

/// A `#[compute(...)]` function.
struct Compute {
    name: syn::Ident,
    is_override: bool,
    is_async: bool,
    /// `#[compute(candidate, ...)]`: an async compute that receives the read-only `ReadTx`.
    candidate: bool,
    depends: Vec<syn::LitStr>,
    reads: Vec<syn::LitStr>,
    depends_context: Vec<syn::LitStr>,
    line_span: Span,
    /// The whole signature: type errors of the generated call point at the function.
    sig_span: Span,
}

fn str_list(meta: &Meta) -> syn::Result<Vec<syn::LitStr>> {
    crate::attrs::array(value(meta)?)?
        .into_iter()
        .map(lit_str)
        .collect()
}

/// Parse `#[compute(depends = [...], reads = [...], depends_context = [...], override)]` and check
/// the signature; rewrites the `sup` parameter type of overrides.
fn parse_compute(f: &mut syn::ImplItemFn, pos: usize, rt: &TokenStream) -> syn::Result<Compute> {
    let attr = f.attrs.remove(pos);
    // `override` is a reserved keyword, so it is not a `Meta`: take it out of the tokens first.
    let mut is_override = false;
    let mut candidate = false;
    let mut candidate_span = attr.span();
    let metas = match &attr.meta {
        Meta::Path(_) => Vec::new(),
        Meta::List(l) => {
            let mut kept: Vec<proc_macro2::TokenTree> = Vec::new();
            let mut skip_comma = false;
            for tt in l.tokens.clone() {
                match &tt {
                    proc_macro2::TokenTree::Ident(i) if i == "override" => {
                        is_override = true;
                        skip_comma = true;
                        continue;
                    }
                    proc_macro2::TokenTree::Ident(i) if i == "candidate" => {
                        candidate = true;
                        candidate_span = i.span();
                        skip_comma = true;
                        continue;
                    }
                    proc_macro2::TokenTree::Punct(p) if p.as_char() == ',' && skip_comma => {
                        skip_comma = false;
                        continue;
                    }
                    _ => skip_comma = false,
                }
                kept.push(tt);
            }
            parse_metas(kept.into_iter().collect())?
        }
        Meta::NameValue(nv) => return err(nv.span(), "expected #[compute(...)]"),
    };
    let mut c = Compute {
        name: f.sig.ident.clone(),
        is_override,
        is_async: f.sig.asyncness.is_some(),
        candidate,
        depends: Vec::new(),
        reads: Vec::new(),
        depends_context: Vec::new(),
        line_span: f.sig.ident.span(),
        sig_span: f.sig.span(),
    };
    for meta in &metas {
        match key(meta).as_str() {
            "depends" => c.depends = str_list(meta)?,
            "reads" => c.reads = str_list(meta)?,
            "depends_context" => c.depends_context = str_list(meta)?,
            other => return err(meta.span(), format!("unknown compute parameter `{other}`")),
        }
    }
    for dep in &c.depends {
        if dep.value().split('.').any(|seg| seg == "id") {
            return err(dep.span(), "Compute method cannot depend on field 'id'.");
        }
    }
    if c.candidate && !c.is_async {
        return err(
            candidate_span,
            "`candidate` applies to async computes only: write `async fn(this: &RecordSet<M>, tx: &mut ReadTx<'_>, out: &mut Assign<'_, M>)`",
        );
    }
    if c.candidate && c.is_override {
        return err(
            candidate_span,
            "a `candidate` compute cannot be an override and has no `super()`",
        );
    }
    let expected = match (c.is_async, c.is_override) {
        (false, false) => 1,
        (false, true) => 2,
        (true, false) => 3,
        (true, true) => 4,
    };
    if f.sig.inputs.len() != expected {
        return err(
            f.sig.span(),
            match (c.is_async, c.is_override) {
                (false, false) => "expected fn(batch: &mut Batch<'_, M>) -> Result<()>",
                (false, true) => {
                    "expected fn(batch: &mut Batch<'_, M>, sup: ComputeSuper) -> Result<()>"
                }
                (true, false) if c.candidate => {
                    "expected async fn(this: &RecordSet<M>, tx: &mut ReadTx<'_>, out: &mut Assign<'_, M>) -> Result<()>"
                }
                (true, false) => {
                    "expected async fn(this: &RecordSet<M>, tx: &mut Transaction, out: &mut Assign<'_, M>) -> Result<()>"
                }
                (true, true) => {
                    "expected async fn(this: &RecordSet<M>, tx: &mut Transaction, out: &mut Assign<'_, M>, sup: ComputeSuper) -> Result<()>"
                }
            },
        );
    }
    if c.candidate
        && let Some(FnArg::Typed(t)) = f.sig.inputs.iter().nth(1)
        && type_names_transaction(&t.ty)
    {
        return err(
            t.ty.span(),
            "a `candidate` compute receives the read-only view `&mut ReadTx<'_>`, not `&mut Transaction`: it cannot mutate, use extensions or queue commit hooks",
        );
    }
    if c.is_override
        && let Some(FnArg::Typed(t)) = f.sig.inputs.last_mut()
    {
        let span = t.ty.span();
        *t.ty = if c.is_async {
            syn::parse2(quote_spanned!(span=> #rt::AsyncComputeSuper))?
        } else {
            syn::parse2(quote_spanned!(span=> #rt::ComputeSuper))?
        };
    }
    Ok(c)
}

/// True when `ty` is `Transaction` or `&mut Transaction` (by its last path segment).
fn type_names_transaction(ty: &Type) -> bool {
    match ty {
        Type::Reference(r) => type_names_transaction(&r.elem),
        Type::Path(p) => p
            .path
            .segments
            .last()
            .is_some_and(|s| s.ident == "Transaction"),
        _ => false,
    }
}

fn result_inner(ret: &ReturnType) -> syn::Result<Type> {
    let ReturnType::Type(_, ty) = ret else {
        return err(ret.span(), "a model method returns erp_sdk::Result<T>");
    };
    if let Type::Path(p) = &**ty
        && let Some(last) = p.path.segments.last()
        && last.ident == "Result"
        && let PathArguments::AngleBracketed(a) = &last.arguments
        && let Some(GenericArgument::Type(t)) = a.args.first()
    {
        return Ok(t.clone());
    }
    err(ty.span(), "a model method returns erp_sdk::Result<T>")
}

fn is_super(ty: &Type) -> bool {
    matches!(ty, Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "Super"))
}

/// What [`rpc_entry`] needs from the method being expanded.
struct RpcEntry<'a> {
    rt: &'a TokenStream,
    rpc: &'a Rpc,
    method: &'a Method,
    target: &'a Path,
    addon: &'a TokenStream,
    arg_names: &'a [syn::Ident],
    arg_types: &'a [&'a Type],
    a_ty: &'a TokenStream,
}

/// The `RpcImpl` registration (and binder) of one `#[rpc]` method.
fn rpc_entry(e: RpcEntry<'_>) -> TokenStream {
    let RpcEntry {
        rt,
        rpc,
        method: m,
        target,
        addon,
        arg_names,
        arg_types,
        a_ty,
    } = e;
    let name = &m.name;
    let name_str = name.to_string();
    let ret = &m.ret;
    let span = m.line_span;
    let rstat = match rpc {
        Rpc::Data { .. } => format_ident!("__ERP_DATA_{}", name_str.to_uppercase()),
        _ => format_ident!("__ERP_RPC_{}", name_str.to_uppercase()),
    };
    let (effect, model, defaults, bind) = match rpc {
        Rpc::Hide => (
            None,
            false,
            &[][..],
            format_ident!("__erp_rpc_bind_{}", name),
        ),
        Rpc::Expose {
            effect,
            model,
            defaults,
            ..
        } => (
            Some(effect),
            *model,
            defaults.as_slice(),
            format_ident!("__erp_rpc_bind_{}", name),
        ),
        Rpc::Data { model, defaults } => (
            None,
            *model,
            defaults.as_slice(),
            format_ident!("__erp_data_bind_{}", name),
        ),
    };
    match rpc {
        Rpc::Hide => quote_spanned! {span=>
            #[#rt::linkme::distributed_slice(#rt::RPC_IMPLS)]
            #[linkme(crate = #rt::linkme)]
            static #rstat: #rt::RpcImpl = #rt::RpcImpl {
                addon: #addon,
                model: <#target as #rt::Model>::MODEL_NAME,
                method: #name_str,
                decl: #rt::RpcDecl::Hide,
                file: ::core::file!(),
                line: ::core::line!(),
            };
        },
        Rpc::Expose { .. } | Rpc::Data { .. } => {
            let receiver = if model {
                quote!(#rt::Receiver::Model)
            } else {
                quote!(#rt::Receiver::Records)
            };
            let mut takes = Vec::new();
            let mut specs = Vec::new();
            for (((var, ty), (pat, _)), default) in arg_names
                .iter()
                .zip(arg_types)
                .zip(&m.args)
                .zip(defaults.iter())
            {
                let pname = match pat {
                    syn::Pat::Ident(i) => i.ident.to_string(),
                    // refused while parsing
                    _ => String::new(),
                };
                match default {
                    Some(expr) => {
                        takes.push(quote! {
                            let #var: #ty = match __args
                                .take::<::core::option::Option<#ty>>(#pname)?
                            {
                                ::core::option::Option::Some(__v) => __v,
                                ::core::option::Option::None => #expr,
                            };
                        });
                        specs.push(quote! {
                            #rt::ParamSpec::with_shape(
                                #pname,
                                false,
                                <#ty as #rt::FromRpc>::EXPECTED,
                                <#ty as #rt::FromRpc>::SHAPE,
                            )
                        });
                    }
                    None => {
                        takes.push(quote! {
                            let #var: #ty = __args.take::<#ty>(#pname)?;
                        });
                        specs.push(quote! {
                            #rt::ParamSpec::with_shape(
                                #pname,
                                <#ty as #rt::FromRpc>::REQUIRED,
                                <#ty as #rt::FromRpc>::EXPECTED,
                                <#ty as #rt::FromRpc>::SHAPE,
                            )
                        });
                    }
                }
            }
            let decl = match effect {
                Some(effect) => quote! {
                    #rt::RpcDecl::Expose {
                        effect: #rt::Effect::#effect,
                        receiver: #receiver,
                        params: &[#(#specs),*],
                        bind: #bind,
                    }
                },
                None => quote! {
                    #rt::RpcDecl::DataCallable {
                        receiver: #receiver,
                        params: &[#(#specs),*],
                        bind: #bind,
                    }
                },
            };
            let offer = match rpc {
                Rpc::Expose {
                    assistant: true, ..
                } => {
                    let ostat = format_ident!("__ERP_ASSISTANT_OFFER_{}", name_str.to_uppercase());
                    quote_spanned! {span=>
                        #[#rt::linkme::distributed_slice(#rt::ASSISTANT_OFFERS)]
                        #[linkme(crate = #rt::linkme)]
                        static #ostat: #rt::AssistantOffer = #rt::AssistantOffer {
                            addon: #addon,
                            model: <#target as #rt::Model>::MODEL_NAME,
                            method: #name_str,
                            file: ::core::file!(),
                            line: ::core::line!(),
                        };
                    }
                }
                Rpc::Expose { .. } | Rpc::Hide | Rpc::Data { .. } => TokenStream::new(),
            };
            quote_spanned! {span=>
                #offer
                #[allow(non_snake_case)]
                fn #bind<'a>(
                    __tx: &'a mut #rt::Transaction,
                    __records: #rt::Records,
                    mut __args: #rt::RpcArgs,
                ) -> #rt::BoxFuture<'a, ::core::result::Result<#rt::Value, #rt::BindError>> {
                    ::std::boxed::Box::pin(async move {
                        #(#takes)*
                        let __r: #ret = #rt::call_method::<#a_ty, #ret>(
                            __tx,
                            &__records,
                            #name_str,
                            (#(#arg_names,)*),
                        )
                        .await?;
                        ::core::result::Result::Ok(#rt::IntoRpc::into_rpc(__r)?)
                    })
                }
                #[#rt::linkme::distributed_slice(#rt::RPC_IMPLS)]
                #[linkme(crate = #rt::linkme)]
                static #rstat: #rt::RpcImpl = #rt::RpcImpl {
                    addon: #addon,
                    model: <#target as #rt::Model>::MODEL_NAME,
                    method: #name_str,
                    decl: #decl,
                    file: ::core::file!(),
                    line: ::core::line!(),
                };
            }
        }
    }
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let rt = quote!(::erp_sdk::__private);
    let metas = parse_metas(attr)?;
    let Some(Meta::Path(target)) = metas.first() else {
        return err(Span::call_site(), "expected #[methods(Model)]");
    };
    let target: Path = target.clone();
    let mut addon = None;
    for meta in metas.iter().skip(1) {
        match key(meta).as_str() {
            "addon" => addon = Some(lit_str(value(meta)?)?),
            other => return err(meta.span(), format!("unknown parameter `{other}`")),
        }
    }
    let addon = addon_tokens(addon);
    let mut block: ItemImpl = syn::parse2(item)?;
    let mut selections = Vec::new();
    if block.trait_.is_some() {
        return err(block.span(), "#[methods] goes on an inherent impl block");
    }
    let self_ty = block.self_ty.clone();
    let self_name = match &*self_ty {
        Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        other => return err(other.span(), "expected a type name"),
    };

    let mut methods = Vec::new();
    let mut computes = Vec::new();
    let mut field_accesses: Vec<FieldAccess> = Vec::new();
    let mut access_domains: Vec<(syn::Ident, Span)> = Vec::new();
    let mut search_methods: Vec<SearchMethod> = Vec::new();
    for item in &mut block.items {
        let ImplItem::Fn(f) = item else { continue };
        if let Some(pos) = f
            .attrs
            .iter()
            .position(|a| a.path().is_ident("field_access"))
        {
            if f.attrs.iter().any(|a| {
                a.path().is_ident("method")
                    || a.path().is_ident("compute")
                    || a.path().is_ident("constrains")
                    || a.path().is_ident("onchange")
                    || a.path().is_ident("rpc")
                    || a.path().is_ident("data_callable")
                    || a.path().is_ident("access_domain")
                    || a.path().is_ident("search_method")
            }) {
                return err(
                    f.sig.ident.span(),
                    "a function is either a #[field_access] or a #[method]/#[compute]/\
                     #[constrains]/#[onchange]/#[rpc]/#[access_domain]/#[search_method], not both",
                );
            }
            field_accesses.push(parse_field_access(f, pos, &rt)?);
            continue;
        }
        if let Some(pos) = f
            .attrs
            .iter()
            .position(|a| a.path().is_ident("access_domain"))
        {
            let attr = f.attrs.remove(pos);
            if !matches!(attr.meta, Meta::Path(_)) {
                return err(attr.span(), "expected #[access_domain] without arguments");
            }
            if f.attrs.iter().any(|a| {
                [
                    "method",
                    "compute",
                    "constrains",
                    "onchange",
                    "rpc",
                    "data_callable",
                    "access_domain",
                    "field_access",
                    "search_method",
                ]
                .iter()
                .any(|n| a.path().is_ident(n))
            }) {
                return err(
                    f.sig.ident.span(),
                    "an #[access_domain] function is only that: not a #[method], #[compute], \
                     #[constrains], #[onchange], #[rpc] or #[data_callable], and once per \
                     function",
                );
            }
            if f.sig.asyncness.is_some() {
                return err(
                    f.sig.fn_token.span(),
                    "#[access_domain] is synchronous: fn(ctx: &AccessCtx, op: Operation, sup: \
                     AccessSuper) -> Result<Domain> (it runs while access is compiled, no I/O)",
                );
            }
            if f.sig.inputs.len() != 3
                || f.sig.inputs.iter().any(|i| matches!(i, FnArg::Receiver(_)))
            {
                return err(
                    f.sig.span(),
                    "expected fn(ctx: &AccessCtx, op: Operation, sup: AccessSuper) -> \
                     Result<Domain>",
                );
            }
            access_domains.push((f.sig.ident.clone(), f.sig.ident.span()));
            continue;
        }
        if let Some(pos) = f
            .attrs
            .iter()
            .position(|a| a.path().is_ident("search_method"))
        {
            if f.attrs.iter().any(|a| {
                [
                    "method",
                    "compute",
                    "constrains",
                    "onchange",
                    "rpc",
                    "data_callable",
                    "access_domain",
                    "field_access",
                    "selection",
                ]
                .iter()
                .any(|n| a.path().is_ident(n))
            }) {
                return err(
                    f.sig.ident.span(),
                    "a function is either a #[search_method] or another attribute, not both",
                );
            }
            search_methods.push(parse_search_method(f, pos, &rt)?);
            continue;
        }
        if let Some(pos) = f.attrs.iter().position(|a| a.path().is_ident("selection")) {
            let attr = f.attrs.remove(pos);
            let is_override = match &attr.meta {
                Meta::Path(_) => false,
                Meta::List(list) if list.tokens.to_string() == "override" => true,
                _ => {
                    return err(
                        attr.span(),
                        "expected #[selection] or #[selection(override)]",
                    );
                }
            };
            if f.attrs.iter().any(|a| {
                [
                    "selection",
                    "search_method",
                    "method",
                    "compute",
                    "constrains",
                    "onchange",
                    "rpc",
                    "data_callable",
                    "access_domain",
                    "field_access",
                ]
                .iter()
                .any(|name| a.path().is_ident(name))
            }) {
                return err(
                    f.sig.span(),
                    "a #[selection] provider cannot carry other method annotations",
                );
            }
            let ret = result_inner(&f.sig.output)?;
            if f.sig.asyncness.is_none()
                || f.sig.inputs.len() != if is_override { 3 } else { 2 }
                || !matches!(ret, Type::Tuple(ref tuple) if tuple.elems.is_empty())
                || f.sig
                    .inputs
                    .iter()
                    .any(|arg| matches!(arg, FnArg::Receiver(_)))
            {
                return err(
                    f.sig.span(),
                    "expected async fn(this: &RecordSet<M>, ctx: &mut SelectionContext<'_>[, sup: SelectionSuper]) -> Result<()>",
                );
            }
            selections.push((f.sig.ident.clone(), is_override, f.sig.ident.span()));
            continue;
        }
        if let Some(pos) = f.attrs.iter().position(|a| a.path().is_ident("compute")) {
            if f.attrs.iter().any(|a| {
                a.path().is_ident("method")
                    || a.path().is_ident("constrains")
                    || a.path().is_ident("onchange")
                    || a.path().is_ident("search_method")
            }) {
                return err(
                    f.sig.ident.span(),
                    "a function is either a #[compute] or a #[method]/#[constrains]/#[onchange], \
                     not both",
                );
            }
            computes.push(parse_compute(f, pos, &rt)?);
            continue;
        }
        let constrains = match f.attrs.iter().position(|a| a.path().is_ident("constrains")) {
            Some(pos) => {
                let attr = f.attrs.remove(pos);
                let names = attr
                    .parse_args_with(
                        syn::punctuated::Punctuated::<syn::LitStr, syn::Token![,]>::parse_terminated,
                    )?
                    .into_iter()
                    .collect::<Vec<_>>();
                if names.is_empty() {
                    return err(attr.span(), "expected #[constrains(\"field\", ...)]");
                }
                Some(names)
            }
            None => None,
        };
        let onchange = match f.attrs.iter().position(|a| a.path().is_ident("onchange")) {
            Some(pos) => {
                let attr = f.attrs.remove(pos);
                let names = attr
                    .parse_args_with(
                        syn::punctuated::Punctuated::<syn::LitStr, syn::Token![,]>::parse_terminated,
                    )?
                    .into_iter()
                    .collect::<Vec<_>>();
                if names.is_empty() {
                    return err(attr.span(), "expected #[onchange(\"field\", ...)]");
                }
                if constrains.is_some() {
                    return err(
                        attr.span(),
                        "a method is either a #[constrains] or an #[onchange], not both",
                    );
                }
                Some(names)
            }
            None => None,
        };
        let rpc_attr = match f.attrs.iter().position(|a| a.path().is_ident("rpc")) {
            Some(pos) => {
                let attr = f.attrs.remove(pos);
                if f.attrs.iter().any(|a| a.path().is_ident("rpc")) {
                    return err(attr.span(), "one #[rpc(...)] per method");
                }
                if !f.attrs.iter().any(|a| a.path().is_ident("method")) {
                    return err(
                        attr.span(),
                        "#[rpc] goes on a #[method] or #[method(override)] function",
                    );
                }
                if constrains.is_some() || onchange.is_some() {
                    return err(
                        attr.span(),
                        "a #[constrains] or #[onchange] method cannot be exposed over RPC",
                    );
                }
                if f.sig.ident.to_string().starts_with('_') {
                    return err(
                        f.sig.ident.span(),
                        "a method whose name starts with `_` is private and cannot be exposed \
                         over RPC",
                    );
                }
                Some(parse_rpc(&attr)?)
            }
            None => None,
        };
        let data_attr = match f
            .attrs
            .iter()
            .position(|a| a.path().is_ident("data_callable"))
        {
            Some(pos) => {
                let attr = f.attrs.remove(pos);
                if f.attrs.iter().any(|a| a.path().is_ident("data_callable")) {
                    return err(attr.span(), "one #[data_callable] per method");
                }
                if !f.attrs.iter().any(|a| a.path().is_ident("method")) {
                    return err(
                        attr.span(),
                        "#[data_callable] goes on a #[method] or #[method(override)] function",
                    );
                }
                if constrains.is_some() || onchange.is_some() {
                    return err(
                        attr.span(),
                        "a #[constrains] or #[onchange] method cannot be data-callable",
                    );
                }
                if matches!(
                    &rpc_attr,
                    Some(RpcAttr {
                        effect: Some(_),
                        hide: false,
                        ..
                    })
                ) {
                    return err(
                        attr.span(),
                        "a method with #[rpc(effect = …)] is data-callable already; drop \
                         #[data_callable]",
                    );
                }
                Some(parse_data_callable(&attr)?)
            }
            None => None,
        };
        let (is_override, wire, package_rpc) = match f
            .attrs
            .iter()
            .position(|a| a.path().is_ident("method"))
        {
            Some(pos) => {
                let flags = match &f.attrs[pos].meta {
                    Meta::Path(_) => (false, false, false),
                    Meta::List(list) => {
                        let words = list.parse_args_with(|input: syn::parse::ParseStream| {
                            syn::punctuated::Punctuated::<syn::Ident, syn::Token![,]>::parse_terminated_with(
                                input,
                                syn::ext::IdentExt::parse_any,
                            )
                        })?;
                        let mut flags = (false, false, false);
                        for word in &words {
                            match word.to_string().as_str() {
                                "override" => flags.0 = true,
                                "wire" => flags.1 = true,
                                "rpc" => flags.2 = true,
                                _ => {
                                    return err(
                                        word.span(),
                                        "expected #[method], #[method(override)], \
                                         #[method(wire)], #[method(rpc)] or a combination",
                                    );
                                }
                            }
                        }
                        flags
                    }
                    other => {
                        return err(other.span(), "expected #[method] or #[method(override)]");
                    }
                };
                f.attrs.remove(pos);
                flags
            }
            // a constraint or onchange method is a method of its own
            None if constrains.is_some() || onchange.is_some() => (false, false, false),
            None => continue,
        };
        if package_rpc && is_override {
            return err(
                f.sig.ident.span(),
                "`rpc` marks a method an addon defines for RPC clients; an override is never \
                 exposed (the definition's declaration decides): remove `rpc`",
            );
        }
        if package_rpc && f.sig.ident.to_string().starts_with('_') {
            return err(
                f.sig.ident.span(),
                "`rpc` on a method whose name starts with `_`: such a name is private and never \
                 callable from a client; rename the method or remove `rpc`",
            );
        }
        if f.sig.asyncness.is_none() {
            return err(f.sig.fn_token.span(), "model methods are `async fn`");
        }
        let ret = result_inner(&f.sig.output)?;
        // `#[arg(default = …)]` belongs to `#[rpc]` methods; taken off before the function is
        // emitted
        let mut arg_defaults: Vec<Option<syn::Expr>> = Vec::new();
        for input in f.sig.inputs.iter_mut() {
            if let FnArg::Typed(t) = input {
                arg_defaults.push(take_arg_default(t)?);
            }
        }
        let inputs: Vec<&mut FnArg> = f.sig.inputs.iter_mut().collect();
        let min = if is_override { 3 } else { 2 };
        if inputs.len() < min {
            return err(
                f.sig.span(),
                if is_override {
                    "expected (this: &RecordSet<M>, tx: &mut Transaction, args..., sup: Super)"
                } else {
                    "expected (this: &RecordSet<M>, tx: &mut Transaction, args...)"
                },
            );
        }
        let mut typed: Vec<(syn::Pat, Type)> = Vec::new();
        for arg in inputs.iter() {
            match &**arg {
                FnArg::Typed(t) => typed.push(((*t.pat).clone(), (*t.ty).clone())),
                FnArg::Receiver(r) => {
                    return err(r.span(), "model methods take `this`, not `self`");
                }
            }
        }
        let n = typed.len();
        let args: Vec<(syn::Pat, Type)> = if is_override {
            if !is_super(&typed[n - 1].1) {
                return err(
                    typed[n - 1].1.span(),
                    "the last parameter of a #[method(override)] is `sup: Super`",
                );
            }
            typed[2..n - 1].to_vec()
        } else {
            typed[2..].to_vec()
        };
        if onchange.is_some() && !args.is_empty() {
            return err(
                f.sig.span(),
                "an #[onchange] method takes (this: &RecordSet<M>, tx: &mut Transaction) and \
                 returns Result<OnchangeResult>",
            );
        }
        let has_default = arg_defaults.iter().any(Option::is_some);
        // the defaults of `args` (after `this` and `tx`; `sup` has none)
        let arg_defaults: Vec<Option<syn::Expr>> =
            arg_defaults.into_iter().skip(2).take(args.len()).collect();
        let data = match data_attr {
            Some(model) => {
                for (pat, _) in &args {
                    let syn::Pat::Ident(ident) = pat else {
                        return err(pat.span(), "#[data_callable] parameters are plain names");
                    };
                    let name = ident.ident.to_string();
                    if name == "ids" || name == "context" {
                        return err(
                            ident.span(),
                            format!(
                                "a #[data_callable] parameter cannot be named `{name}`: it is a \
                                 reserved key of the call body"
                            ),
                        );
                    }
                }
                Some(Rpc::Data {
                    model,
                    defaults: arg_defaults.clone(),
                })
            }
            None => None,
        };
        let rpc = match rpc_attr {
            Some(RpcAttr {
                effect: Some(effect),
                model,
                hide: false,
                assistant,
            }) => {
                for (pat, _) in &args {
                    let syn::Pat::Ident(ident) = pat else {
                        return err(pat.span(), "#[rpc] parameters are plain names");
                    };
                    let name = ident.ident.to_string();
                    if name == "ids" || name == "context" {
                        return err(
                            ident.span(),
                            format!(
                                "a #[rpc] parameter cannot be named `{name}`: it is a reserved \
                                 key of the call body"
                            ),
                        );
                    }
                }
                Some(Rpc::Expose {
                    effect,
                    model,
                    assistant,
                    defaults: arg_defaults,
                })
            }
            Some(_) => {
                if has_default && data.is_none() {
                    return err(
                        f.sig.span(),
                        "#[arg(default = …)] needs #[rpc(effect = …)] or #[data_callable]",
                    );
                }
                Some(Rpc::Hide)
            }
            None => {
                if has_default && data.is_none() {
                    return err(
                        f.sig.span(),
                        "#[arg(default = …)] needs #[rpc(effect = …)] or #[data_callable]",
                    );
                }
                None
            }
        };
        let arg_types: Vec<&Type> = args.iter().map(|(_, t)| t).collect();
        if is_override {
            // Fill in Super's generics.
            if let Some(FnArg::Typed(t)) = f.sig.inputs.last_mut() {
                let span = t.ty.span();
                *t.ty = syn::parse2(quote_spanned!(span=> #rt::Super<(#(#arg_types,)*), #ret>))?;
            }
        }
        methods.push(Method {
            name: f.sig.ident.clone(),
            is_override,
            wire,
            package_rpc,
            args,
            ret,
            line_span: f.sig.ident.span(),
            constrains,
            onchange,
            rpc,
            data,
        });
    }

    let mut entries = Vec::new();
    let mut trait_items = Vec::new();
    let mut trait_impls = Vec::new();
    // the same trait for a component: its calls go to the host in the wire form
    let mut guest_trait_items = Vec::new();
    let mut guest_trait_impls = Vec::new();
    for m in &methods {
        let name = &m.name;
        let name_str = name.to_string();
        let ret = &m.ret;
        let arg_types: Vec<&Type> = m.args.iter().map(|(_, t)| t).collect();
        let arg_names: Vec<syn::Ident> = (0..m.args.len())
            .map(|i| format_ident!("__a{}", i))
            .collect();
        let a_ty = quote!((#(#arg_types,)*));
        let entry = format_ident!("__erp_entry_{}", name);
        let erased = format_ident!("__erp_erased_{}", name);
        let stat = format_ident!("__ERP_IMPL_{}", name.to_string().to_uppercase());
        let kind = if m.is_override {
            quote!(#rt::ImplKind::Override)
        } else {
            quote!(#rt::ImplKind::Define)
        };
        let call = if m.is_override {
            quote!(<#self_ty>::#name(&__this, __tx, #(#arg_names,)* __sup).await)
        } else {
            quote!({ let _ = __sup; <#self_ty>::#name(&__this, __tx, #(#arg_names,)*).await })
        };
        let span = m.line_span;
        if name_str == "_adapt_generated_input" && !m.is_override {
            entries.push(quote_spanned! {span=>
                #[#rt::linkme::distributed_slice(#rt::GENERATED_INPUT_ADAPTER_DECLS)]
                #[linkme(crate = #rt::linkme)]
                static __ERP_GENERATED_INPUT_ADAPTER: #rt::GeneratedInputAdapterDecl =
                    #rt::GeneratedInputAdapterDecl::new(#addon, <#target as #rt::Model>::MODEL_NAME);
            });
        }
        entries.push(quote_spanned! {span=>
            #[allow(non_snake_case)]
            fn #entry<'a>(
                __tx: &'a mut #rt::Transaction,
                __records: &'a #rt::Records,
                __args: #a_ty,
                __sup: #rt::Super<#a_ty, #ret>,
            ) -> #rt::BoxFuture<'a, #rt::Result<#ret>> {
                ::std::boxed::Box::pin(async move {
                    // Checked: the records' model is the target model or inherits it.
                    let __this: #rt::RecordSet<#target> = ::core::convert::TryFrom::try_from(
                        ::core::clone::Clone::clone(__records),
                    )?;
                    let (#(#arg_names,)*) = __args;
                    #call
                })
            }
            #[allow(non_snake_case)]
            fn #erased() -> &'static (dyn ::core::any::Any + ::core::marker::Send + ::core::marker::Sync) {
                static F: #rt::MethodFn<#a_ty, #ret> = #entry;
                &F
            }
            #[#rt::linkme::distributed_slice(#rt::METHOD_IMPLS)]
            #[linkme(crate = #rt::linkme)]
            static #stat: #rt::MethodImpl = #rt::MethodImpl {
                addon: #addon,
                model: <#target as #rt::Model>::MODEL_NAME,
                method: #name_str,
                kind: #kind,
                sig_name: ::core::any::type_name::<#rt::MethodFn<#a_ty, #ret>>,
                entry: #erased,
                file: ::core::file!(),
                line: ::core::line!(),
            };
        });
        if m.wire {
            let cstat = format_ident!("__ERP_CODEC_{}", name.to_string().to_uppercase());
            entries.push(quote_spanned! {span=>
                #[#rt::linkme::distributed_slice(#rt::METHOD_CODECS)]
                #[linkme(crate = #rt::linkme)]
                static #cstat: #rt::MethodCodecImpl = #rt::MethodCodecImpl {
                    addon: #addon,
                    model: <#target as #rt::Model>::MODEL_NAME,
                    method: #name_str,
                    codec: &#rt::TypedCodec::<#a_ty, #ret>::new(),
                };
            });
        }
        if let Some(fields) = &m.constrains {
            let cstat = format_ident!("__ERP_CONSTRAINT_{}", name.to_string().to_uppercase());
            entries.push(quote_spanned! {span=>
                #[#rt::linkme::distributed_slice(#rt::CONSTRAINT_IMPLS)]
                #[linkme(crate = #rt::linkme)]
                static #cstat: #rt::ConstraintImpl = #rt::ConstraintImpl {
                    addon: #addon,
                    model: <#target as #rt::Model>::MODEL_NAME,
                    method: #name_str,
                    fields: &[#(#fields),*],
                    file: ::core::file!(),
                    line: ::core::line!(),
                };
            });
        }
        if let Some(fields) = &m.onchange {
            let ostat = format_ident!("__ERP_ONCHANGE_{}", name.to_string().to_uppercase());
            entries.push(quote_spanned! {span=>
                #[#rt::linkme::distributed_slice(#rt::ONCHANGE_IMPLS)]
                #[linkme(crate = #rt::linkme)]
                static #ostat: #rt::OnchangeImpl = #rt::OnchangeImpl {
                    addon: #addon,
                    model: <#target as #rt::Model>::MODEL_NAME,
                    method: #name_str,
                    fields: &[#(#fields),*],
                    file: ::core::file!(),
                    line: ::core::line!(),
                };
            });
        }
        for rpc in m.rpc.iter().chain(m.data.iter()) {
            entries.push(rpc_entry(RpcEntry {
                rt: &rt,
                rpc,
                method: m,
                target: &target,
                addon: &addon,
                arg_names: &arg_names,
                arg_types: &arg_types,
                a_ty: &a_ty,
            }));
        }
        if !m.is_override {
            let params: Vec<TokenStream> = arg_names
                .iter()
                .zip(&arg_types)
                .map(|(n, t)| quote!(#n: #t))
                .collect();
            trait_items.push(quote! {
                #[allow(missing_docs)]
                fn #name<'a>(&'a self, tx: &'a mut #rt::Transaction, #(#params),*)
                    -> #rt::BoxFuture<'a, #rt::Result<#ret>>;
            });
            trait_impls.push(quote! {
                fn #name<'a>(&'a self, tx: &'a mut #rt::Transaction, #(#params),*)
                    -> #rt::BoxFuture<'a, #rt::Result<#ret>> {
                    #rt::call_method::<#a_ty, #ret>(tx, self.as_records(), #name_str, (#(#arg_names,)*))
                }
            });
            // constraints and onchange handlers are not called by name
            if m.constrains.is_none() && m.onchange.is_none() {
                guest_trait_items.push(quote! {
                    #[allow(missing_docs)]
                    async fn #name(&self, tx: &mut #rt::Transaction, #(#params),*)
                        -> #rt::Result<#ret>;
                });
                guest_trait_impls.push(quote! {
                    async fn #name(&self, tx: &mut #rt::Transaction, #(#params),*)
                        -> #rt::Result<#ret> {
                        #rt::call_by_name::<_, #a_ty, #ret>(tx, self, #name_str, (#(#arg_names,)*)).await
                    }
                });
            }
        }
    }

    for (name, is_override, span) in &selections {
        let name_str = name.to_string();
        let entry = format_ident!("__erp_selection_entry_{}", name);
        let erased = format_ident!("__erp_selection_erased_{}", name);
        let stat = format_ident!("__ERP_SELECTION_{}", name.to_string().to_uppercase());
        let kind = if *is_override {
            quote!(#rt::ImplKind::Override)
        } else {
            quote!(#rt::ImplKind::Define)
        };
        let call = if *is_override {
            quote!(<#self_ty>::#name(&__this, __ctx, __sup).await)
        } else {
            quote!({ let _ = __sup; <#self_ty>::#name(&__this, __ctx).await })
        };
        entries.push(quote_spanned! {*span=>
            #[allow(non_snake_case)]
            fn #entry<'a, 'tx>(
                __ctx: &'a mut #rt::SelectionContext<'tx>,
                __records: &'a #rt::Records,
                __sup: #rt::SelectionSuper,
            ) -> #rt::BoxFuture<'a, #rt::Result<()>> {
                ::std::boxed::Box::pin(async move {
                    let __this: #rt::RecordSet<#target> = ::core::convert::TryFrom::try_from(
                        ::core::clone::Clone::clone(__records),
                    )?;
                    #call
                })
            }
            #[allow(non_snake_case)]
            fn #erased() -> &'static (dyn ::core::any::Any + ::core::marker::Send + ::core::marker::Sync) {
                static F: #rt::SelectionFn = #entry;
                &F
            }
            #[#rt::linkme::distributed_slice(#rt::METHOD_IMPLS)]
            #[linkme(crate = #rt::linkme)]
            static #stat: #rt::MethodImpl = #rt::MethodImpl {
                addon: #addon,
                model: <#target as #rt::Model>::MODEL_NAME,
                method: #name_str,
                kind: #kind,
                sig_name: ::core::any::type_name::<#rt::SelectionFn>,
                entry: #erased,
                file: ::core::file!(),
                line: ::core::line!(),
            };
        });
    }

    for fa in &field_accesses {
        let name = &fa.name;
        let name_str = name.to_string();
        let span = fa.line_span;
        let entry = format_ident!("__erp_field_access_entry_{}", name);
        let stat = format_ident!("__ERP_FIELD_ACCESS_{}", name.to_string().to_uppercase());
        let context = &fa.context;
        entries.push(quote_spanned! {span=>
            #[allow(non_snake_case)]
            fn #entry<'c, 'a, 's>(
                __ctx: &'c #rt::FieldAccessCtx<'a>,
                __op: #rt::FieldOp,
                __mask: #rt::FieldMask,
                __sup: #rt::FieldAccessSuper<'s>,
            ) -> #rt::Result<#rt::FieldMask> {
                <#self_ty>::#name(__ctx, __op, __mask, __sup)
            }
            #[#rt::linkme::distributed_slice(#rt::FIELD_ACCESS_IMPLS)]
            #[linkme(crate = #rt::linkme)]
            static #stat: #rt::FieldAccessImpl = #rt::FieldAccessImpl {
                addon: #addon,
                model: <#target as #rt::Model>::MODEL_NAME,
                method: #name_str,
                context: &[#(#context),*],
                run: #entry,
                file: ::core::file!(),
                line: ::core::line!(),
            };
        });
    }

    for c in &computes {
        let name = &c.name;
        let name_str = name.to_string();
        let span = c.line_span;
        let entry = format_ident!("__erp_compute_entry_{}", name);
        let stat = format_ident!("__ERP_COMPUTE_{}", name.to_string().to_uppercase());
        let kind = if c.is_override {
            quote!(#rt::ImplKind::Override)
        } else {
            quote!(#rt::ImplKind::Define)
        };
        let (fn_def, entry_value) = if c.candidate {
            (
                quote_spanned! {span=>
                    #[allow(non_snake_case)]
                    fn #entry<'a, 't>(
                        __tx: &'a mut #rt::ReadTx<'t>,
                        __records: &'a #rt::Records,
                        __out: &'a mut #rt::Outputs,
                    ) -> #rt::BoxFuture<'a, #rt::Result<()>> {
                        ::std::boxed::Box::pin(async move {
                            let __this: #rt::RecordSet<#target> = ::core::convert::TryFrom::try_from(
                                ::core::clone::Clone::clone(__records),
                            )?;
                            let mut __out = #rt::Assign::<#target>::wrap(__out);
                            <#self_ty>::#name(&__this, __tx, &mut __out).await
                        })
                    }
                },
                quote!(#rt::ComputeEntry::Candidate(#entry)),
            )
        } else if c.is_async {
            let call = if c.is_override {
                quote_spanned!(c.sig_span=> <#self_ty>::#name(&__this, __tx, &mut __out, __sup).await)
            } else {
                quote_spanned!(c.sig_span=> { let _ = __sup; <#self_ty>::#name(&__this, __tx, &mut __out).await })
            };
            (
                quote_spanned! {span=>
                    #[allow(non_snake_case)]
                    fn #entry<'a>(
                        __tx: &'a mut #rt::Transaction,
                        __records: &'a #rt::Records,
                        __out: &'a mut #rt::Outputs,
                        __sup: #rt::AsyncComputeSuper,
                    ) -> #rt::BoxFuture<'a, #rt::Result<()>> {
                        ::std::boxed::Box::pin(async move {
                            let __this: #rt::RecordSet<#target> = ::core::convert::TryFrom::try_from(
                                ::core::clone::Clone::clone(__records),
                            )?;
                            let mut __out = #rt::Assign::<#target>::wrap(__out);
                            #call
                        })
                    }
                },
                quote!(#rt::ComputeEntry::Async(#entry)),
            )
        } else {
            let call = if c.is_override {
                quote_spanned!(c.sig_span=> <#self_ty>::#name(&mut __batch, __sup))
            } else {
                quote_spanned!(c.sig_span=> { let _ = __sup; <#self_ty>::#name(&mut __batch) })
            };
            (
                quote_spanned! {span=>
                    #[allow(non_snake_case)]
                    fn #entry(
                        __data: &mut #rt::BatchData,
                        __sup: #rt::ComputeSuper,
                    ) -> #rt::Result<()> {
                        let mut __batch = #rt::Batch::<#target>::wrap(__data)?;
                        #call
                    }
                },
                quote!(#rt::ComputeEntry::Sync(#entry)),
            )
        };
        let depends = &c.depends;
        let reads = &c.reads;
        let depends_context = &c.depends_context;
        entries.push(quote_spanned! {span=>
            #fn_def
            #[#rt::linkme::distributed_slice(#rt::COMPUTE_IMPLS)]
            #[linkme(crate = #rt::linkme)]
            static #stat: #rt::ComputeImpl = #rt::ComputeImpl {
                addon: #addon,
                model: <#target as #rt::Model>::MODEL_NAME,
                method: #name_str,
                kind: #kind,
                depends: &[#(#depends),*],
                reads: &[#(#reads),*],
                depends_context: &[#(#depends_context),*],
                entry: #entry_value,
                file: ::core::file!(),
                line: ::core::line!(),
            };
        });
    }

    for (name, span) in &access_domains {
        let stat = format_ident!("__ERP_ACCESS_DOMAIN_{}", name.to_string().to_uppercase());
        entries.push(quote_spanned! {*span=>
            #[#rt::linkme::distributed_slice(#rt::ACCESS_DOMAIN_IMPLS)]
            #[linkme(crate = #rt::linkme)]
            static #stat: #rt::AccessDomainImpl = #rt::AccessDomainImpl {
                addon: #addon,
                model: <#target as #rt::Model>::MODEL_NAME,
                func: <#self_ty>::#name,
                file: ::core::file!(),
                line: ::core::line!(),
            };
        });
    }

    for sm in &search_methods {
        let name = &sm.name;
        let span = sm.span;
        let stat = format_ident!("__ERP_FIELD_SEARCH_{}", name.to_string().to_uppercase());
        let wrapper = format_ident!("__erp_search_wrapper_{}", name);
        let wrapper_call = if sm.takes_super {
            quote_spanned! {span=>
                <#self_ty>::#name(req, ctx, sup)
            }
        } else {
            quote_spanned! {span=>
                let _ = sup;
                <#self_ty>::#name(req, ctx)
            }
        };
        let method_str = name.to_string();
        entries.push(quote_spanned! {span=>
            #[allow(non_snake_case)]
            fn #wrapper(
                req: &#rt::FieldSearchRequest<'_>,
                ctx: &mut #rt::FieldSearchContext<'_>,
                sup: #rt::FieldSearchSuper<'_>,
            ) -> #rt::Result<#rt::Domain> {
                #wrapper_call
            }

            #[#rt::linkme::distributed_slice(#rt::FIELD_SEARCH_IMPLS)]
            #[linkme(crate = #rt::linkme)]
            static #stat: #rt::FieldSearchImpl = #rt::FieldSearchImpl {
                addon: #addon,
                model: <#target as #rt::Model>::MODEL_NAME,
                method: #method_str,
                func: #wrapper,
                file: ::core::file!(),
                line: ::core::line!(),
            };
        });
    }

    let guest_trait_def = if guest_trait_items.is_empty() {
        quote!()
    } else {
        let trait_name = format_ident!("{}Methods", self_name);
        let doc = format!(
            "Methods defined by `{self_name}`, callable on recordsets of the model and of models inheriting it."
        );
        quote! {
            #[cfg(target_family = "wasm")]
            #[doc = #doc]
            #[allow(async_fn_in_trait)]
            pub trait #trait_name {
                #(#guest_trait_items)*
            }
            #[cfg(target_family = "wasm")]
            impl<__M: #rt::Inherits<#target>> #trait_name for #rt::RecordSet<__M> {
                #(#guest_trait_impls)*
            }
        }
    };
    let trait_def = if trait_items.is_empty() {
        quote!()
    } else {
        let trait_name = format_ident!("{}Methods", self_name);
        let doc = format!(
            "Methods defined by `{self_name}`, callable on recordsets of the model and of models inheriting it."
        );
        // typed calls go through the ORM's call path: native only until
        quote! {
            #[cfg(not(target_family = "wasm"))]
            #[doc = #doc]
            pub trait #trait_name {
                #(#trait_items)*
            }
            #[cfg(not(target_family = "wasm"))]
            impl<__M: #rt::Inherits<#target>> #trait_name for #rt::RecordSet<__M> {
                #(#trait_impls)*
            }
        }
    };

    // The backend-neutral description (/003) and the refusals for what a component
    // cannot do yet.
    let model_name = quote!(<#target as #rt::Model>::MODEL_NAME);
    let mut registers = Vec::new();
    // Native capability hooks follow field_access/access_domain: no ordinary portable
    // method descriptor or unimplemented component invocation contract is advertised.
    for (_, _, span) in &selections {
        registers.push(quote_spanned! {*span=>
            #[cfg(target_family = "wasm")]
            compile_error!("#[selection] requires native SelectionContext capabilities; untrusted providers need a separate script/Wasm execution budget");
        });
    }
    for m in &methods {
        let name = m.name.to_string();
        let args: Vec<serde_json::Value> = m
            .args
            .iter()
            .map(|(pat, ty)| {
                serde_json::json!({
                    "name": crate::descriptor::text(pat),
                    "type": crate::descriptor::text(ty),
                })
            })
            .collect();
        let mut detail = serde_json::json!({
            "args": args,
            "ret": crate::descriptor::text(&m.ret),
        });
        if m.package_rpc {
            detail["rpc"] = serde_json::Value::Bool(true);
        }
        // a constraint is a method of the component the host calls after a write or create
        if m.onchange.is_none() {
            registers.push(guest_registration(
                &rt,
                &addon,
                &model_name,
                &self_ty,
                &target,
                m,
            ));
        }
        let kind = if m.is_override { "Override" } else { "Method" };
        registers.push(crate::descriptor::register(
            &addon,
            kind,
            &model_name,
            &name,
            &detail,
        ));
        for (kind, fields) in [("Constraint", &m.constrains), ("Onchange", &m.onchange)] {
            if let Some(fields) = fields {
                let detail = serde_json::json!({ "fields": crate::descriptor::lits_json(fields) });
                registers.push(crate::descriptor::register(
                    &addon,
                    kind,
                    &model_name,
                    &name,
                    &detail,
                ));
            }
        }
        if m.rpc.is_some() {
            registers.push(crate::descriptor::wasm_unsupported(m.line_span, "#[rpc]"));
        }
        if m.data.is_some() {
            registers.push(crate::descriptor::wasm_unsupported(
                m.line_span,
                "#[data_callable]",
            ));
        }
    }
    for c in &computes {
        let detail = serde_json::json!({
            "async": c.is_async,
            "depends": crate::descriptor::lits_json(&c.depends),
            "reads": crate::descriptor::lits_json(&c.reads),
            "depends_context": crate::descriptor::lits_json(&c.depends_context),
        });
        let kind = if c.is_override {
            "ComputeOverride"
        } else {
            "Compute"
        };
        registers.push(crate::descriptor::register(
            &addon,
            kind,
            &model_name,
            &c.name.to_string(),
            &detail,
        ));
        // A component runs a synchronous compute over declared plain fields; the host hands it the
        // inputs as data and calls it as a method of the component named like the compute. The
        // others (async, override, a path through a relation) stay described only: installing a
        // package that has one is refused by name (`checks_from_describe`).
        if !c.is_async
            && !c.is_override
            && !c
                .depends
                .iter()
                .chain(&c.reads)
                .any(|path| path.value().contains('.'))
        {
            registers.push(guest_compute_registration(
                &rt,
                &addon,
                &model_name,
                &self_ty,
                &target,
                c,
            ));
        }
    }
    for fa in &field_accesses {
        registers.push(crate::descriptor::wasm_unsupported(
            fa.line_span,
            "#[field_access]",
        ));
    }
    for (_, span) in &access_domains {
        registers.push(crate::descriptor::wasm_unsupported(
            *span,
            "#[access_domain]",
        ));
    }
    for sm in &search_methods {
        registers.push(crate::descriptor::wasm_unsupported(
            sm.span,
            "#[search_method]",
        ));
    }
    // the functions are called through the native registrations only
    block
        .attrs
        .push(syn::parse_quote!(#[cfg_attr(target_family = "wasm", allow(dead_code))]));

    Ok(quote! {
        #block
        #trait_def
        #guest_trait_def
        #[cfg(not(target_family = "wasm"))]
        const _: () = {
            #(#entries)*
        };
        #(#registers)*
    })
}

/// The component's entry for one synchronous compute (`cfg(target_family = "wasm")` only).
fn guest_compute_registration(
    rt: &TokenStream,
    addon: &TokenStream,
    model_name: &TokenStream,
    self_ty: &Type,
    target: &Path,
    c: &Compute,
) -> TokenStream {
    let name = &c.name;
    let name_str = name.to_string();
    let run = format_ident!("__erp_guest_run_compute_{}", name);
    let span = c.line_span;
    // an anonymous const scope: two models of one addon may have a compute of the same name
    quote_spanned! {span=>
        #[cfg(target_family = "wasm")]
        const _: () = {
            #[allow(non_snake_case)]
            fn #run(
                __ctx: &#rt::CallContext,
                __ids: ::std::vec::Vec<i64>,
                __args: &str,
            ) -> ::core::result::Result<::std::string::String, #rt::BoundaryError> {
                let _ = (__ctx, __ids);
                #rt::run_compute::<#target, _>(#name_str, __args, <#self_ty>::#name)
            }
            #rt::inventory::submit! {
                #rt::GuestMethod {
                    addon: #addon,
                    model: #model_name,
                    method: #name_str,
                    run: #run,
                }
            }
        };
    }
}

/// The component's entry for one method (`cfg(target_family = "wasm")` only): decodes the JSON
/// arguments, runs the function on the call's records and encodes the result.
fn guest_registration(
    rt: &TokenStream,
    addon: &TokenStream,
    model_name: &TokenStream,
    self_ty: &Type,
    target: &Path,
    m: &Method,
) -> TokenStream {
    let name = &m.name;
    let name_str = name.to_string();
    let ret = &m.ret;
    let arg_types: Vec<&Type> = m.args.iter().map(|(_, t)| t).collect();
    let arg_names: Vec<syn::Ident> = (0..m.args.len())
        .map(|i| format_ident!("__a{}", i))
        .collect();
    let a_ty = quote!((#(#arg_types,)*));
    let run = format_ident!("__erp_guest_run_{}", name);
    let call = if m.is_override {
        quote!(<#self_ty>::#name(&__this, &mut __tx, #(#arg_names,)* __sup).await)
    } else {
        quote!({ let _ = __sup; <#self_ty>::#name(&__this, &mut __tx, #(#arg_names,)*).await })
    };
    let span = m.line_span;
    // an anonymous const scope: two models of one addon may have a method of the same name (a
    // Return button on the book and on the loan)
    quote_spanned! {span=>
        #[cfg(target_family = "wasm")]
        const _: () = {
            #[allow(non_snake_case)]
            fn #run(
                __ctx: &#rt::CallContext,
                __ids: ::std::vec::Vec<i64>,
                __args: &str,
            ) -> ::core::result::Result<::std::string::String, #rt::BoundaryError> {
                #rt::run_method::<#a_ty, #ret, _, _>(
                    __ctx,
                    __ids,
                    __args,
                    #name_str,
                    |__ids, mut __tx, __args, __sup| async move {
                        let __this = #rt::RecordSet::<#target>::__from_ids(__ids);
                        let (#(#arg_names,)*) = __args;
                        #call
                    },
                )
            }
            #rt::inventory::submit! {
                #rt::GuestMethod {
                    addon: #addon,
                    model: #model_name,
                    method: #name_str,
                    run: #run,
                }
            }
        };
    }
}

#[cfg(test)]
mod search_signature_tests {
    use super::*;
    #[test]
    fn search_method_rejects_wrong_request_context_and_result_types() {
        for (source, message) in [
            (
                "#[search_method] fn search(req: &str, ctx: &mut FieldSearchContext<'_>) -> Result<Domain> { todo!() }",
                "#[search_method] parameter 1 must be &FieldSearchRequest<'_>",
            ),
            (
                "#[search_method] fn search(req: &FieldSearchRequest<'_>, ctx: &FieldSearchContext<'_>) -> Result<Domain> { todo!() }",
                "#[search_method] parameter 2 must be &mut FieldSearchContext<'_>",
            ),
            (
                "#[search_method] fn search(req: &FieldSearchRequest<'_>, ctx: &mut FieldSearchContext<'_>) -> Result<bool> { todo!() }",
                "#[search_method] must return Result<Domain>",
            ),
        ] {
            let mut function: syn::ImplItemFn = syn::parse_str(source).unwrap();
            let error = parse_search_method(&mut function, 0, &quote!(::erp_sdk::__private))
                .err()
                .expect("the signature must produce a syn::Error");
            assert_eq!(error.to_string(), message, "{source}");
        }
    }
    #[test]
    fn search_method_rejects_mismatched_super_type() {
        let mut function: syn::ImplItemFn = syn::parse_quote! {
            #[search_method(override)]
            fn search(req: &FieldSearchRequest<'_>, ctx: &mut FieldSearchContext<'_>, sup: usize) -> Result<Domain> { todo!() }
        };
        let result = parse_search_method(&mut function, 0, &quote!(::erp_sdk::__private));
        let error = result
            .err()
            .expect("the signature must produce a syn::Error");
        assert_eq!(
            error.to_string(),
            "#[search_method] third parameter must be FieldSearchSuper<'_>"
        );
    }
}
