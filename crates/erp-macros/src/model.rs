//! `#[model]`, `#[extend]` and `addon!`.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Expr, Fields, ItemStruct, Meta, Path};

use crate::attrs::{
    array, err, flag, key, lit_bool, lit_str, lit_u32, parse_metas, some_str, tuple, value,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    New,
    Extend,
}

/// Path prefix of the runtime support module.
pub(crate) fn rt() -> TokenStream {
    quote!(::erp_sdk::__private)
}

/// `addon = "x"` or the crate's `__ERP_ADDON`.
pub(crate) fn addon_tokens(addon: Option<syn::LitStr>) -> TokenStream {
    match addon {
        Some(a) => quote!(#a),
        None => quote!(crate::__ERP_ADDON),
    }
}

struct FieldOut {
    decl: TokenStream,
    marker: TokenStream,
}

fn index_tokens(meta: &Meta, field_kind: &str) -> syn::Result<TokenStream> {
    let rt = rt();
    let kind = match meta {
        Meta::Path(_) => "btree".to_owned(),
        Meta::NameValue(nv) => match &nv.value {
            Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Bool(b),
                ..
            }) => {
                if b.value {
                    "btree".to_owned()
                } else {
                    return Ok(quote!(.index(::core::option::Option::None)));
                }
            }
            other => lit_str(other)?.value(),
        },
        Meta::List(l) => return err(l.span(), "expected `index` or `index = \"...\"`"),
    };
    let variant = match kind.as_str() {
        "btree" => quote!(Btree),
        "btree_not_null" => quote!(BtreeNotNull),
        "trigram" => quote!(Trigram),
        "brin" => quote!(Brin),
        "gin" => quote!(Gin),
        other => return err(meta.span(), format!("unknown index kind {other:?}")),
    };
    if let Some(reason) = crate::typed::field_kind_mismatch(&kind, field_kind) {
        return err(meta.span(), reason);
    }
    Ok(quote!(.index(::core::option::Option::Some(#rt::IndexKind::#variant))))
}

fn default_tokens(expr: &Expr) -> syn::Result<TokenStream> {
    let rt = rt();
    Ok(match expr {
        Expr::Lit(syn::ExprLit { lit, .. }) => match lit {
            syn::Lit::Str(s) => quote!(.default(#rt::StaticValue::Str(#s))),
            syn::Lit::Int(i) => quote!(.default(#rt::StaticValue::Int(#i))),
            syn::Lit::Float(f) => quote!(.default(#rt::StaticValue::Float(#f))),
            syn::Lit::Bool(b) => quote!(.default(#rt::StaticValue::Bool(#b))),
            other => return err(other.span(), "unsupported default literal"),
        },
        Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => match &*u.expr {
            Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Int(i),
                ..
            }) => quote!(.default(#rt::StaticValue::Int(-#i))),
            Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Float(f),
                ..
            }) => quote!(.default(#rt::StaticValue::Float(-#f))),
            other => return err(other.span(), "unsupported default"),
        },
        Expr::Path(p) if p.path.is_ident("none") => quote!(.no_default()),
        Expr::Path(p) if p.path.is_ident("False") => quote!(.default(#rt::StaticValue::False)),
        other => {
            return err(
                other.span(),
                "default must be a literal, `none` or `False`; use default_fn for functions",
            );
        }
    })
}

fn selection_pairs(expr: &Expr) -> syn::Result<TokenStream> {
    let mut pairs = Vec::new();
    for item in array(expr)? {
        let parts = tuple(item);
        if parts.len() != 2 {
            return err(item.span(), "expected (\"value\", \"Label\")");
        }
        let (k, v) = (lit_str(parts[0])?, lit_str(parts[1])?);
        pairs.push(quote!((#k, #v)));
    }
    Ok(quote!(&[#(#pairs),*]))
}

fn selection_add(expr: &Expr) -> syn::Result<TokenStream> {
    let mut pairs = Vec::new();
    for item in array(expr)? {
        let parts = tuple(item);
        match parts.as_slice() {
            [k] => {
                let k = lit_str(k)?;
                pairs.push(quote!((#k, ::core::option::Option::None)));
            }
            [k, v] => {
                let (k, v) = (lit_str(k)?, lit_str(v)?);
                pairs.push(quote!((#k, ::core::option::Option::Some(#v))));
            }
            _ => {
                return err(
                    item.span(),
                    "expected (\"value\", \"Label\") or (\"value\",)",
                );
            }
        }
    }
    Ok(quote!(&[#(#pairs),*]))
}

fn selection_ondelete_tokens(expr: &Expr) -> syn::Result<TokenStream> {
    let rt = rt();
    let mut policies = Vec::new();
    let mut keys = std::collections::BTreeSet::new();
    for item in array(expr)? {
        let parts = tuple(item);
        let [key, policy] = parts.as_slice() else {
            return err(
                item.span(),
                "expected (\"selection key\", \"cleanup policy\")",
            );
        };
        let key = lit_str(key)?;
        if !keys.insert(key.value()) {
            return err(key.span(), "selection cleanup key is given twice");
        }
        let policy = lit_str(policy)?;
        let variant = match policy.value().as_str() {
            "set null" => quote!(SetNull),
            "cascade" => quote!(Cascade),
            "set default" => quote!(SetDefault),
            _ => {
                return err(
                    policy.span(),
                    "selection ondelete must be 'set null', 'cascade' or 'set default'",
                );
            }
        };
        policies.push(quote!((#key, #rt::SelectionOnDeleteDecl::#variant)));
    }
    Ok(quote!(&[#(#policies),*]))
}

fn ondelete_tokens(meta: Option<&Meta>) -> syn::Result<TokenStream> {
    let rt = rt();
    Ok(match meta {
        Some(m) => {
            let v = lit_str(value(m)?)?;
            let variant = match v.value().as_str() {
                "set null" => quote!(SetNull),
                "restrict" => quote!(Restrict),
                "cascade" => quote!(Cascade),
                other => return err(v.span(), format!("unknown ondelete {other:?}")),
            };
            quote!(::core::option::Option::Some(#rt::OnDelete::#variant))
        }
        None => quote!(::core::option::Option::None),
    })
}

/// A JSON string literal.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A domain written in Rust literal syntax (`[("name", "ilike", "a"), "|", ...]`) as JSON list
/// syntax, checked when the ORM starts.
fn json_of(expr: &Expr) -> syn::Result<String> {
    Ok(match expr {
        Expr::Array(a) => {
            let items = a
                .elems
                .iter()
                .map(json_of)
                .collect::<syn::Result<Vec<_>>>()?;
            format!("[{}]", items.join(", "))
        }
        Expr::Tuple(t) => {
            let items = t
                .elems
                .iter()
                .map(json_of)
                .collect::<syn::Result<Vec<_>>>()?;
            format!("[{}]", items.join(", "))
        }
        Expr::Paren(p) => json_of(&p.expr)?,
        Expr::Lit(syn::ExprLit { lit, .. }) => match lit {
            syn::Lit::Str(s) => json_str(&s.value()),
            syn::Lit::Bool(b) => b.value.to_string(),
            syn::Lit::Int(i) => i.base10_digits().to_owned(),
            syn::Lit::Float(f) => f.base10_digits().to_owned(),
            other => return err(other.span(), "unsupported literal in a domain"),
        },
        Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => format!("-{}", json_of(&u.expr)?),
        other => {
            return err(
                other.span(),
                "expected a domain literal: [(\"field\", \"op\", value), ...]",
            );
        }
    })
}

/// A context written as `[("active_test", false)]` as a JSON object.
fn json_object_of(expr: &Expr) -> syn::Result<String> {
    let mut parts = Vec::new();
    for item in array(expr)? {
        let pair = tuple(item);
        let [k, v] = pair.as_slice() else {
            return err(item.span(), "expected (\"key\", value)");
        };
        parts.push(format!(
            "{}: {}",
            json_str(&lit_str(k)?.value()),
            json_of(v)?
        ));
    }
    Ok(format!("{{{}}}", parts.join(", ")))
}

/// `tracking = true | false | <sequence>`: a bare `tracking` is `true`.
fn tracking_tokens(meta: &Meta) -> syn::Result<TokenStream> {
    let rt = rt();
    let expr = match meta {
        Meta::Path(_) => {
            return Ok(quote!(.tracking(#rt::DEFAULT_TRACKING_SEQUENCE)));
        }
        other => value(other)?,
    };
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Bool(b),
            ..
        }) => Ok(if b.value {
            quote!(.tracking(#rt::DEFAULT_TRACKING_SEQUENCE))
        } else {
            quote!(.untracked())
        }),
        Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(i),
            ..
        }) => {
            let n: u32 = i.base10_parse()?;
            Ok(quote!(.tracking(#n)))
        }
        other => err(
            other.span(),
            "tracking takes `true`, `false` or an integer display sequence",
        ),
    }
}

fn parse_field(name: &str, attr: &syn::Attribute) -> syn::Result<FieldOut> {
    let rt = rt();
    let metas = parse_metas(attr.parse_args::<TokenStream>()?)?;
    let Some((first, rest)) = metas.split_first() else {
        return err(attr.span(), "expected a field kind, e.g. #[field(char)]");
    };
    let Meta::Path(kind_path) = first else {
        return err(first.span(), "the first argument must be the field kind");
    };
    let kind = kind_path
        .get_ident()
        .map(ToString::to_string)
        .unwrap_or_default();

    let mut kind_opts: Vec<&Meta> = Vec::new();
    let mut calls: Vec<TokenStream> = Vec::new();
    for meta in rest {
        match key(meta).as_str() {
            k @ ("string" | "help" | "compute" | "inverse" | "related" | "groups"
            | "search_method" | "collation") => {
                let s = lit_str(value(meta)?)?;
                let m = format_ident!("{}", k);
                calls.push(quote!(.#m(#s)));
            }
            k @ ("required"
            | "readonly"
            | "store"
            | "copy"
            | "prefetch"
            | "precompute"
            | "compute_sudo"
            | "check_company"
            | "company_dependent"
            | "change_default"
            | "user_writeable"
            | "input_adapted"
            | "secret"
            | "bypass_search_access"
            | "delegate"
            | "server_set"
            | "translate"
            | "db_default"
            | "sanitize") => {
                let b = flag(meta)?;
                let m = format_ident!("{}", k);
                calls.push(quote!(.#m(#b)));
            }
            k @ ("depends" | "reads" | "depends_context") => {
                let items = array(value(meta)?)?
                    .into_iter()
                    .map(lit_str)
                    .collect::<syn::Result<Vec<_>>>()?;
                let m = format_ident!("{}", k);
                calls.push(quote!(.#m(&[#(#items),*])));
            }
            k @ ("domain" | "context") => {
                let v = value(meta)?;
                let text = match v {
                    Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) => s.value(),
                    other if k == "domain" => json_of(other)?,
                    other => json_object_of(other)?,
                };
                let lit = syn::LitStr::new(&text, v.span());
                let m = format_ident!("{}", k);
                calls.push(quote!(.#m(#lit)));
            }
            "tracking" => calls.push(tracking_tokens(meta)?),
            "index" => calls.push(index_tokens(meta, &kind)?),
            "default" => calls.push(default_tokens(value(meta)?)?),
            "default_fn" => {
                let f = value(meta)?;
                calls.push(quote!(.default_fn(#f)));
            }
            "generated" => {
                let s = lit_str(value(meta)?)?;
                calls.push(quote!(.generated(#s)));
            }
            "generated_storage" => {
                let s = lit_str(value(meta)?)?;
                let storage_val = match s.value().to_ascii_lowercase().as_str() {
                    "virtual" => quote!(#rt::GeneratedStorage::Virtual),
                    "stored" => quote!(#rt::GeneratedStorage::Stored),
                    other => {
                        return err(
                            s.span(),
                            format!(
                                "unknown generated_storage `{other}`, expected `virtual` or `stored`"
                            ),
                        );
                    }
                };
                calls.push(quote!(.generated_storage(#storage_val)));
            }
            "digits" | "size" | "currency_field" | "selection" | "selection_method"
            | "selection_add" | "comodel" | "ondelete" | "inverse_name" | "relation"
            | "column1" | "column2" | "model_field" | "bounds" => kind_opts.push(meta),
            other => return err(meta.span(), format!("unknown field parameter `{other}`")),
        }
    }
    let opt = |k: &str| kind_opts.iter().copied().find(|m| key(m) == k);

    let (kind_tokens, marker) = match kind.as_str() {
        "boolean" => (quote!(#rt::KindDecl::Boolean), quote!(Boolean)),
        "bigint" => (quote!(#rt::KindDecl::BigInt), quote!(BigInt)),
        "integer" => (quote!(#rt::KindDecl::Integer), quote!(Integer)),
        "float" => match opt("digits") {
            None => (
                quote!(#rt::KindDecl::Float { digits: ::core::option::Option::None }),
                quote!(Float),
            ),
            Some(m) => {
                let v = value(m)?;
                let digits = match v {
                    Expr::Tuple(_) | Expr::Paren(_) => {
                        let parts = tuple(v);
                        let [p, s] = parts.as_slice() else {
                            return err(v.span(), "expected digits = (precision, scale)");
                        };
                        let (p, s) = (lit_u32(p)?, lit_u32(s)?);
                        quote!(#rt::Digits::Fixed { precision: #p, scale: #s })
                    }
                    Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) => quote!(#rt::Digits::Named(#s)),
                    Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Bool(b),
                        ..
                    }) if !b.value => quote!(#rt::Digits::Unrounded),
                    Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Int(i),
                        ..
                    }) if i.base10_digits() == "0" => quote!(#rt::Digits::Unrounded),
                    other => {
                        return err(
                            other.span(),
                            "expected digits = (precision, scale), 0, false or \"Name\"",
                        );
                    }
                };
                (
                    quote!(#rt::KindDecl::Float { digits: ::core::option::Option::Some(#digits) }),
                    quote!(Numeric),
                )
            }
        },
        "monetary" => {
            let cf = match opt("currency_field") {
                Some(m) => some_str(&lit_str(value(m)?)?),
                None => quote!(::core::option::Option::None),
            };
            (
                quote!(#rt::KindDecl::Monetary { currency_field: #cf }),
                quote!(Numeric),
            )
        }
        "char" => {
            let size = match opt("size") {
                Some(m) => {
                    let n = lit_u32(value(m)?)?;
                    quote!(::core::option::Option::Some(#n))
                }
                None => quote!(::core::option::Option::None),
            };
            (quote!(#rt::KindDecl::Char { size: #size }), quote!(Text))
        }
        "text" => (quote!(#rt::KindDecl::Text), quote!(Text)),
        "html" => (quote!(#rt::KindDecl::Html), quote!(Text)),
        "selection" => {
            let values = match opt("selection") {
                Some(m) => {
                    let pairs = selection_pairs(value(m)?)?;
                    quote!(::core::option::Option::Some(#pairs))
                }
                None => quote!(::core::option::Option::None),
            };
            let method = match opt("selection_method") {
                Some(m) => some_str(&lit_str(value(m)?)?),
                None => quote!(::core::option::Option::None),
            };
            let add = match opt("selection_add") {
                Some(m) => selection_add(value(m)?)?,
                None => quote!(&[]),
            };
            let ondelete = match opt("ondelete") {
                Some(meta) => selection_ondelete_tokens(value(meta)?)?,
                None => quote!(&[]),
            };
            (
                quote!(#rt::KindDecl::Selection { values: #values, method: #method, add: #add, ondelete: #ondelete }),
                quote!(Text),
            )
        }
        "date" => (quote!(#rt::KindDecl::Date), quote!(Date)),
        "datetime" => (quote!(#rt::KindDecl::Datetime), quote!(Datetime)),
        "uuid" => (quote!(#rt::KindDecl::Uuid), quote!(Uuid)),
        "xid8" => (quote!(#rt::KindDecl::Xid8), quote!(Xid8)),
        "timestamptz" => (quote!(#rt::KindDecl::TimestampTz), quote!(TimestampTz)),
        "json" => (quote!(#rt::KindDecl::Json), quote!(Json)),
        "many2one" => {
            let Some(m) = opt("comodel") else {
                return err(attr.span(), "many2one needs comodel = \"model.name\"");
            };
            let comodel = lit_str(value(m)?)?;
            let ondelete = ondelete_tokens(opt("ondelete"))?;
            (
                quote!(#rt::KindDecl::Many2one { comodel: #comodel, ondelete: #ondelete }),
                quote!(Many2one),
            )
        }
        "one2many" => {
            let Some(m) = opt("comodel") else {
                return err(attr.span(), "one2many needs comodel = \"model.name\"");
            };
            let comodel = lit_str(value(m)?)?;
            let Some(m) = opt("inverse_name") else {
                return err(attr.span(), "one2many needs inverse_name = \"field\"");
            };
            let inverse = lit_str(value(m)?)?;
            (
                quote!(#rt::KindDecl::One2many { comodel: #comodel, inverse: #inverse }),
                quote!(X2many),
            )
        }
        "many2many" => {
            let Some(m) = opt("comodel") else {
                return err(attr.span(), "many2many needs comodel = \"model.name\"");
            };
            let comodel = lit_str(value(m)?)?;
            let text = |k: &str| -> syn::Result<TokenStream> {
                Ok(match opt(k) {
                    Some(m) => some_str(&lit_str(value(m)?)?),
                    None => quote!(::core::option::Option::None),
                })
            };
            let (relation, column1, column2) =
                (text("relation")?, text("column1")?, text("column2")?);
            let ondelete = ondelete_tokens(opt("ondelete"))?;
            (
                quote!(#rt::KindDecl::Many2many {
                    comodel: #comodel,
                    relation: #relation,
                    column1: #column1,
                    column2: #column2,
                    ondelete: #ondelete,
                }),
                quote!(X2many),
            )
        }
        "reference" => {
            let models = match opt("selection") {
                Some(m) => {
                    let pairs = selection_pairs(value(m)?)?;
                    quote!(::core::option::Option::Some(#pairs))
                }
                None => quote!(::core::option::Option::None),
            };
            let method = match opt("selection_method") {
                Some(m) => some_str(&lit_str(value(m)?)?),
                None => quote!(::core::option::Option::None),
            };
            (
                quote!(#rt::KindDecl::Reference { models: #models, method: #method }),
                quote!(Text),
            )
        }
        "many2one_reference" => {
            let Some(m) = opt("model_field") else {
                return err(
                    attr.span(),
                    "many2one_reference needs model_field = \"field\"",
                );
            };
            let model_field = lit_str(value(m)?)?;
            (
                quote!(#rt::KindDecl::Many2oneReference { model_field: #model_field }),
                quote!(Many2oneReference),
            )
        }
        "date_range" | "datetime_range" => {
            let Some(m) = opt("bounds") else {
                return err(
                    attr.span(),
                    format!("{kind} needs bounds = \"[)\" or \"[]\" (no default)"),
                );
            };
            let bounds = crate::excl::bounds_tokens(&lit_str(value(m)?)?)?;
            if kind == "date_range" {
                (
                    quote!(#rt::KindDecl::DateRange { bounds: #bounds }),
                    quote!(DateRange),
                )
            } else {
                (
                    quote!(#rt::KindDecl::DatetimeRange { bounds: #bounds }),
                    quote!(DatetimeRange),
                )
            }
        }
        other => return err(kind_path.span(), format!("unknown field kind `{other}`")),
    };
    for m in &kind_opts {
        let ok = match key(m).as_str() {
            "digits" => kind == "float",
            "size" => kind == "char",
            "currency_field" => kind == "monetary",
            "selection" | "selection_method" => matches!(kind.as_str(), "selection" | "reference"),
            "selection_add" => kind == "selection",
            "comodel" => matches!(kind.as_str(), "many2one" | "one2many" | "many2many"),
            "ondelete" => matches!(kind.as_str(), "many2one" | "many2many" | "selection"),
            "inverse_name" => kind == "one2many",
            "relation" | "column1" | "column2" => kind == "many2many",
            "model_field" => kind == "many2one_reference",
            "bounds" => matches!(kind.as_str(), "date_range" | "datetime_range"),
            _ => false,
        };
        if !ok {
            return err(
                m.span(),
                format!("`{}` does not apply to {kind} fields", key(m)),
            );
        }
    }
    Ok(FieldOut {
        decl: quote!(#rt::FieldDecl::new(#name, #kind_tokens) #(#calls)*),
        marker: quote!(#rt::kinds::#marker),
    })
}

struct ModelArgs {
    name: Option<syn::LitStr>,
    target: Option<Path>,
    description: Option<syn::LitStr>,
    table: Option<syn::LitStr>,
    log_access: Option<bool>,
    rec_name: Option<syn::LitStr>,
    rec_names_search: Option<Vec<syn::LitStr>>,
    active_name: Option<syn::LitStr>,
    order: Option<syn::LitStr>,
    kind: Option<TokenStream>,
    inherit: Vec<Path>,
    addon: Option<syn::LitStr>,
    table_objects: Vec<TokenStream>,
    inherits: Vec<(syn::LitStr, syn::LitStr)>,
    check_company_auto: Option<bool>,
    /// `company_match = "…"` as the `CompanyMatch` variant path.
    company_match: Option<TokenStream>,
    /// `company_access = …` as a `CompanyAccessDecl` expression (A1-D2).
    company_access: Option<TokenStream>,
    allow_sudo_commands: Option<bool>,
    inherits_access: Option<bool>,
    parent_name: Option<syn::LitStr>,
    parent_store: Option<bool>,
    fillfactor: Option<u8>,
    /// `transient_max_age_seconds = N`; `0` disables the threshold.
    transient_max_age_seconds: Option<u64>,
    /// `transient_max_count = N`; `0` disables the threshold.
    transient_max_count: Option<u64>,
    /// `id_sequence = "…"` or `= PATH` to a `&str` constant.
    id_sequence: Option<Expr>,
    partition: Option<TokenStream>,
    // (D3)
    row_version: Option<bool>,
    eventful: Option<bool>,
    audit_reads: Option<bool>,
    /// Business keys: `(declared name, associated constant name)`.
    keys: Vec<(syn::LitStr, syn::Ident)>,
    field_parameters: Option<Vec<syn::LitStr>>,
    /// `owner_only_stored_fields = ["co_owner", ...]`; the registry refuses
    /// it on an extension.
    owner_only_stored_fields: Option<Vec<syn::LitStr>>,
    extensions: Vec<TokenStream>,
}

/// The value of a transient policy attribute: a decimal integer literal without sign or suffix.
/// The WebAssembly description carries the same source text and applies the same grammar
/// (`erp-modules` `policy_integer`); the registry checks the bounds when it is built.
fn policy_literal(attr: &str, expr: &Expr) -> syn::Result<u64> {
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(i),
            ..
        }) => {
            let text = i.to_string();
            if text.starts_with('-') {
                return err(
                    expr.span(),
                    format!("`{attr}` must not be negative (0 disables the threshold)"),
                );
            }
            if !text.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
                return err(
                    expr.span(),
                    format!(
                        "`{attr}` takes a decimal integer literal without suffix, e.g. \
                         `{attr} = 3600` (0 disables the threshold)"
                    ),
                );
            }
            i.base10_parse::<u64>()
                .or_else(|_| err(expr.span(), format!("`{attr}` does not fit in 64 bits")))
        }
        Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Neg(_),
            ..
        }) => err(
            expr.span(),
            format!("`{attr}` must not be negative (0 disables the threshold)"),
        ),
        other => err(
            other.span(),
            format!(
                "`{attr}` takes a decimal integer literal, e.g. `{attr} = 3600` (0 disables the \
                 threshold)"
            ),
        ),
    }
}

fn parse_model_args(attr: TokenStream, mode: Mode) -> syn::Result<ModelArgs> {
    let rt = rt();
    let metas = parse_metas(attr)?;
    let mut args = ModelArgs {
        name: None,
        target: None,
        description: None,
        table: None,
        log_access: None,
        rec_name: None,
        rec_names_search: None,
        active_name: None,
        order: None,
        kind: None,
        inherit: Vec::new(),
        addon: None,
        table_objects: Vec::new(),
        inherits: Vec::new(),
        check_company_auto: None,
        company_match: None,
        company_access: None,
        allow_sudo_commands: None,
        inherits_access: None,
        parent_name: None,
        parent_store: None,
        fillfactor: None,
        transient_max_age_seconds: None,
        transient_max_count: None,
        id_sequence: None,
        partition: None,
        row_version: None,
        eventful: None,
        audit_reads: None,
        keys: Vec::new(),
        field_parameters: None,
        owner_only_stored_fields: None,
        extensions: Vec::new(),
    };
    for (i, meta) in metas.iter().enumerate() {
        if mode == Mode::Extend && i == 0 {
            let Meta::Path(p) = meta else {
                return err(
                    meta.span(),
                    "expected the model type to extend: #[extend(Model)]",
                );
            };
            args.target = Some(p.clone());
            continue;
        }
        match key(meta).as_str() {
            "name" => args.name = Some(lit_str(value(meta)?)?),
            "description" => args.description = Some(lit_str(value(meta)?)?),
            "table" => args.table = Some(lit_str(value(meta)?)?),
            "log_access" => args.log_access = Some(lit_bool(value(meta)?)?),
            "eventful" => {
                if args.eventful.is_some() {
                    return err(meta.span(), "`eventful` given twice");
                }
                args.eventful = Some(lit_bool(value(meta)?)?);
            }
            "audit_reads" => {
                if args.audit_reads.is_some() {
                    return err(meta.span(), "`audit_reads` given twice");
                }
                args.audit_reads = Some(lit_bool(value(meta)?)?);
            }
            "rec_name" => args.rec_name = Some(lit_str(value(meta)?)?),
            "rec_names_search" => {
                let names = array(value(meta)?)?
                    .iter()
                    .map(|e| lit_str(e))
                    .collect::<syn::Result<Vec<_>>>()?;
                args.rec_names_search = Some(names);
            }
            "field_parameters" => {
                let names = array(value(meta)?)?
                    .iter()
                    .map(|e| lit_str(e))
                    .collect::<syn::Result<Vec<_>>>()?;
                args.field_parameters = Some(names);
            }
            "owner_only_stored_fields" => {
                if mode == Mode::Extend {
                    return err(
                        meta.span(),
                        "`owner_only_stored_fields` is declared by the model's owner in \
                         #[model], not in #[extend]",
                    );
                }
                if args.owner_only_stored_fields.is_some() {
                    return err(meta.span(), "`owner_only_stored_fields` given twice");
                }
                let names = array(value(meta)?)?
                    .iter()
                    .map(|e| lit_str(e))
                    .collect::<syn::Result<Vec<_>>>()?;
                args.owner_only_stored_fields = Some(names);
            }
            "active_name" => args.active_name = Some(lit_str(value(meta)?)?),
            "order" => args.order = Some(lit_str(value(meta)?)?),
            "addon" => args.addon = Some(lit_str(value(meta)?)?),
            "inherits" => {
                for e in array(value(meta)?)? {
                    let parts = tuple(e);
                    let [m, f] = parts.as_slice() else {
                        return err(e.span(), "expected (\"parent.model\", \"delegate_field\")");
                    };
                    args.inherits.push((lit_str(m)?, lit_str(f)?));
                }
            }
            "check_company_auto" => args.check_company_auto = Some(flag(meta)?),
            "company_match" => {
                let m = lit_str(value(meta)?)?;
                args.company_match = Some(match company_match_variant(&m.value()) {
                    Some(variant) => {
                        let variant = format_ident!("{}", variant, span = m.span());
                        quote!(#rt::CompanyMatch::#variant)
                    }
                    None => {
                        return err(
                            m.span(),
                            "company_match must be one of \"exact\", \"parent_of\", \"any_of\", \"parent_of_any\"",
                        );
                    }
                });
            }
            "company_access" => {
                if args.company_access.is_some() {
                    return err(meta.span(), "company_access is given twice");
                }
                args.company_access = Some(crate::company_access::tokens(value(meta)?)?);
            }
            "allow_sudo_commands" => args.allow_sudo_commands = Some(flag(meta)?),
            "inherits_access" => args.inherits_access = Some(flag(meta)?),
            "parent_name" => args.parent_name = Some(lit_str(value(meta)?)?),
            "parent_store" => args.parent_store = Some(flag(meta)?),
            "fillfactor" => {
                let v = value(meta)?;
                let n = lit_u32(v)?;
                match u8::try_from(n) {
                    Ok(ff @ 10..=100) => args.fillfactor = Some(ff),
                    _ => {
                        return err(
                            v.span(),
                            "fillfactor is a percentage from 10 to 100 (PostgreSQL's range)",
                        );
                    }
                }
            }
            k @ ("transient_max_age_seconds" | "transient_max_count") => {
                let slot = if k == "transient_max_count" {
                    &mut args.transient_max_count
                } else {
                    &mut args.transient_max_age_seconds
                };
                if slot.is_some() {
                    return err(meta.span(), format!("`{k}` is given twice"));
                }
                *slot = Some(policy_literal(k, value(meta)?)?);
            }
            "id_sequence" => {
                if args.id_sequence.is_some() {
                    return err(meta.span(), "`id_sequence` is given twice");
                }
                args.id_sequence = Some(id_sequence_value(value(meta)?)?);
            }
            "partition" => {
                if args.partition.is_some() {
                    return err(meta.span(), "`partition` is given twice");
                }
                args.partition = Some(partition_tokens(value(meta)?)?);
            }
            "kind" => {
                let k = lit_str(value(meta)?)?;
                args.kind = Some(match k.value().as_str() {
                    "regular" => quote!(#rt::ModelKind::Regular),
                    "abstract" => quote!(#rt::ModelKind::Abstract),
                    "transient" => quote!(#rt::ModelKind::Transient),
                    other => {
                        return err(
                            k.span(),
                            format!(
                                "unknown kind {other:?}: use \"regular\", \"abstract\" or \"transient\""
                            ),
                        );
                    }
                });
            }
            "inherit" => {
                for e in array(value(meta)?)? {
                    match e {
                        Expr::Path(p) => args.inherit.push(p.path.clone()),
                        other => return err(other.span(), "expected a model type"),
                    }
                }
            }
            k @ ("constraints" | "indexes" | "unique_indexes") => {
                for e in array(value(meta)?)? {
                    // typed objects use call syntax (design m3-ormx §9)
                    if let Some(obj) = crate::typed::table_object(k, e)? {
                        args.table_objects.push(obj);
                        continue;
                    }
                    let parts = tuple(e);
                    let strs = parts
                        .iter()
                        .map(|p| lit_str(p))
                        .collect::<syn::Result<Vec<_>>>()?;
                    let obj = match (k, strs.as_slice()) {
                        ("constraints", [n, d, m]) => quote!(#rt::TableObjectDecl {
                            name: #n, kind: #rt::TableObjectKind::Constraint { definition: #d, message: #m }
                        }),
                        ("indexes", [n, d]) => quote!(#rt::TableObjectDecl {
                            name: #n, kind: #rt::TableObjectKind::Index { definition: #d }
                        }),
                        ("unique_indexes", [n, d, m]) => quote!(#rt::TableObjectDecl {
                            name: #n, kind: #rt::TableObjectKind::UniqueIndex { definition: #d, message: #m }
                        }),
                        _ => {
                            return err(
                                e.span(),
                                "expected (\"_name\", \"definition\"[, \"message\"])",
                            );
                        }
                    };
                    args.table_objects.push(obj);
                }
            }
            "signal_triggers" => {
                for e in array(value(meta)?)? {
                    args.table_objects.push(signal_trigger(e)?);
                }
            }
            "follow_deletes" => {
                for e in array(value(meta)?)? {
                    args.table_objects.push(follow_delete(e)?);
                }
            }
            // typed objects
            "exclusions" => args
                .table_objects
                .extend(crate::excl::exclusions(value(meta)?)?),
            // `row_version = true` and business keys; a `key(...)` with
            // `without_overlaps` is the temporal key
            "row_version" => args.row_version = Some(flag(meta)?),
            "keys" => {
                let (temporal, business): (Vec<_>, Vec<_>) = array(value(meta)?)?
                    .into_iter()
                    .partition(|e| crate::keys::is_temporal(e));
                args.table_objects.extend(crate::excl::keys(temporal)?);
                for e in business {
                    let (obj, name, ident) = crate::keys::parse_key(e)?;
                    args.table_objects.push(obj);
                    args.keys.push((name, ident));
                }
            }
            "extensions" => args.extensions = crate::excl::extensions(value(meta)?)?,
            other => return err(meta.span(), format!("unknown model parameter `{other}`")),
        }
    }
    match mode {
        Mode::New if args.name.is_none() => err(Span::call_site(), "#[model] needs name = \"...\""),
        Mode::Extend if args.name.is_some() => err(
            Span::call_site(),
            "#[extend] takes the model type; to define a new model use #[model(name = ..., inherit = [...])]",
        ),
        _ => Ok(args),
    }
}

/// `partition(interval = "day", premake = 7, retain = 30 | "all"[, by = "field"])` as a
/// `PartitionDecl` expression. The registry checks the values; here only the syntax.
fn partition_tokens(expr: &Expr) -> syn::Result<TokenStream> {
    let rt = rt();
    const USAGE: &str = "expected partition(interval = \"day\", premake = 7, retain = 30 or \"all\"[, by = \"field\"])";
    let Expr::Call(call) = expr else {
        return err(expr.span(), USAGE);
    };
    if !matches!(&*call.func, Expr::Path(p) if p.path.is_ident("partition")) {
        return err(call.func.span(), USAGE);
    }
    let (mut interval, mut premake, mut retain, mut by) = (None, None, None, None);
    for arg in &call.args {
        let Expr::Assign(assign) = arg else {
            return err(arg.span(), "expected `key = value` inside partition(...)");
        };
        let Some(ident) = (match &*assign.left {
            Expr::Path(path) => path.path.get_ident(),
            _ => None,
        }) else {
            return err(assign.left.span(), "expected a partition parameter name");
        };
        let name = ident.to_string();
        let right = &*assign.right;
        let duplicate = || {
            err(
                arg.span(),
                format!("partition parameter `{name}` is given twice"),
            )
        };
        match name.as_str() {
            "interval" => {
                if interval.is_some() {
                    return duplicate();
                }
                let s = lit_str(right)?;
                interval = Some(match s.value().as_str() {
                    "hour" => quote!(Hour),
                    "day" => quote!(Day),
                    "week" => quote!(Week),
                    "month" => quote!(Month),
                    other => {
                        return err(
                            s.span(),
                            format!(
                                "unknown partition interval {other:?}: use \"hour\", \"day\", \
                                 \"week\" or \"month\""
                            ),
                        );
                    }
                });
            }
            "premake" => {
                if premake.is_some() {
                    return duplicate();
                }
                premake = Some(lit_u32(right)?);
            }
            "retain" => {
                if retain.is_some() {
                    return duplicate();
                }
                retain = Some(match right {
                    Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) if s.value() == "all" => quote!(#rt::PartitionRetain::All),
                    Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Int(_),
                        ..
                    }) => {
                        let n = lit_u32(right)?;
                        quote!(#rt::PartitionRetain::Intervals(#n))
                    }
                    other => {
                        return err(other.span(), "retain is a number of intervals or \"all\"");
                    }
                });
            }
            "by" => {
                if by.is_some() {
                    return duplicate();
                }
                by = Some(lit_str(right)?);
            }
            other => {
                return err(
                    ident.span(),
                    format!("unknown partition parameter `{other}`"),
                );
            }
        }
    }
    let (Some(interval), Some(premake), Some(retain)) = (interval, premake, retain) else {
        return err(
            call.span(),
            "partition(...) needs interval, premake and retain (there are no defaults)",
        );
    };
    let by = by.map(|field| quote!(.by(#field)));
    Ok(quote!(
        #rt::PartitionDecl::new(#rt::PartitionInterval::#interval, #premake, #retain) #by
    ))
}

/// The value of `id_sequence`: a string literal, checked here (a PostgreSQL identifier of at
/// most 63 bytes), or a path to a `&str` constant, checked by a `const` assertion
/// the expansion adds ([`id_sequence_tokens`]).
fn id_sequence_value(expr: &Expr) -> syn::Result<Expr> {
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => {
            check_sequence_name(&s.value()).map_err(|why| syn::Error::new(s.span(), why))?;
            Ok(expr.clone())
        }
        Expr::Path(_) => Ok(expr.clone()),
        other => err(
            other.span(),
            "id_sequence is a string literal or the path of a `&str` constant",
        ),
    }
}

/// Why `name` cannot be an id sequence name (the rule of `ModelDecl::valid_id_sequence`).
fn check_sequence_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if !valid {
        return Err(format!(
            "id_sequence {name:?} is not a PostgreSQL identifier: ASCII letters, digits, '_' and \
             '$', starting with a letter or '_'"
        ));
    }
    if name.len() > 63 {
        return Err(format!(
            "id_sequence {name:?} is {} bytes long; PostgreSQL identifiers have at most 63",
            name.len()
        ));
    }
    Ok(())
}

/// `.id_sequence(…)` with a compile-time check of the name (also for a constant's value).
fn id_sequence_tokens(seq: &Expr) -> TokenStream {
    let rt = rt();
    quote!(.id_sequence({
        const SEQUENCE: &str = #seq;
        const _: () = ::core::assert!(
            #rt::ModelDecl::valid_id_sequence(SEQUENCE),
            "id_sequence is not a PostgreSQL identifier of at most 63 bytes"
        );
        SEQUENCE
    }))
}

/// A `&str` argument of a table object: a string literal or the path of a constant.
fn str_or_path(expr: &Expr, what: &str) -> syn::Result<TokenStream> {
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(_),
            ..
        })
        | Expr::Path(_) => Ok(quote!(#expr)),
        other => err(
            other.span(),
            format!("{what} is a string literal or the path of a `&str` constant"),
        ),
    }
}

/// One `follow_deletes` entry: `("_name", cache = "actions"[, key_table = "ir_actions"]
/// [, relation = "<many2many field>"])`; `cache` and `key_table` may name constants. The registry
/// checks the names when it is built.
fn follow_delete(e: &Expr) -> syn::Result<TokenStream> {
    let rt = rt();
    let usage = "expected (\"_name\", cache = \"cache\"[, key_table = \"table\"][, relation = \
                 \"many2many_field\"])";
    let parts = tuple(e);
    let [name, options @ ..] = parts.as_slice() else {
        return err(e.span(), usage);
    };
    let name = lit_str(name)?;
    let (mut cache, mut key_table, mut relation) = (None, None, None);
    for option in options {
        let Expr::Assign(assign) = option else {
            return err(option.span(), usage);
        };
        let Expr::Path(path) = &*assign.left else {
            return err(assign.left.span(), usage);
        };
        let key = path.path.get_ident().map(ToString::to_string);
        let slot = match key.as_deref() {
            Some("cache") => &mut cache,
            Some("key_table") => &mut key_table,
            Some("relation") => &mut relation,
            _ => return err(assign.left.span(), usage),
        };
        if slot.is_some() {
            return err(
                assign.left.span(),
                format!("`{}` given twice", key.unwrap_or_default()),
            );
        }
        *slot = Some(&*assign.right);
    }
    let Some(cache) = cache else {
        return err(e.span(), "a follow-delete trigger needs `cache = \"...\"`");
    };
    let cache = str_or_path(cache, "cache")?;
    let mut decl = quote!(#rt::TableObjectKind::follow_delete(#cache));
    if let Some(table) = key_table {
        let table = str_or_path(table, "key_table")?;
        decl = quote!(#decl.deleting_keys(#table));
    }
    if let Some(field) = relation {
        let field = lit_str(field)?;
        decl = quote!(#decl.on_relation(#field));
    }
    Ok(quote!(#rt::TableObjectDecl {
        name: #name, kind: #rt::TableObjectKind::FollowDelete(#decl)
    }))
}

/// One `signal_triggers` entry: `("_name", "policy")`, optionally followed by
/// `relation = "<many2many field>"` and/or `update = false`.
fn signal_trigger(e: &Expr) -> syn::Result<TokenStream> {
    let rt = rt();
    let parts = tuple(e);
    let usage =
        "expected (\"_name\", \"policy\"[, relation = \"many2many_field\"][, update = false])";
    let [name, signal, options @ ..] = parts.as_slice() else {
        return err(e.span(), usage);
    };
    let name = lit_str(name)?;
    let signal_lit = lit_str(signal)?;
    let signal = match signal_lit.value().as_str() {
        "policy" => quote!(#rt::TriggerSignal::Policy),
        "membership" => quote!(#rt::TriggerSignal::Membership),
        other => {
            return err(
                signal_lit.span(),
                format!(
                    "unknown signal {other:?}: signal triggers bump \"policy\" or \"membership\""
                ),
            );
        }
    };
    let mut decl = quote!(#rt::SignalTriggerDecl::new(#signal));
    let (mut relation, mut update, mut update_of) = (false, false, false);
    for option in options {
        let Expr::Assign(assign) = option else {
            return err(option.span(), usage);
        };
        let Expr::Path(path) = &*assign.left else {
            return err(assign.left.span(), usage);
        };
        match path.path.get_ident().map(ToString::to_string).as_deref() {
            Some("relation") if !relation => {
                relation = true;
                let field = lit_str(&assign.right)?;
                decl = quote!(#decl.on_relation(#field));
            }
            Some("update") if !update => {
                update = true;
                if lit_bool(&assign.right)? {
                    return err(
                        assign.right.span(),
                        "`update = true` is the default; write `update = false` or leave it out",
                    );
                }
                decl = quote!(#decl.without_updates());
            }
            Some("update_of") if !update_of && !update => {
                update_of = true;
                let columns = array(&assign.right)?
                    .into_iter()
                    .map(lit_str)
                    .collect::<syn::Result<Vec<_>>>()?;
                if columns.is_empty() {
                    return err(assign.right.span(), "`update_of` needs at least one column");
                }
                decl = quote!(#decl.updates_of(&[#(#columns),*]));
            }
            Some(key @ ("relation" | "update" | "update_of")) => {
                return err(assign.left.span(), format!("`{key}` given twice"));
            }
            _ => return err(assign.left.span(), usage),
        }
    }
    Ok(quote!(#rt::TableObjectDecl {
        name: #name, kind: #rt::TableObjectKind::SignalTrigger(#decl)
    }))
}

fn const_name(field: &str) -> syn::Ident {
    format_ident!("{}", field.trim_start_matches("r#").to_uppercase())
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream, mode: Mode) -> syn::Result<TokenStream> {
    let rt = rt();
    // the arguments again, as source text for the descriptor (`extend`'s first one is the target)
    let arg_metas = parse_metas(attr.clone())?;
    let args = parse_model_args(attr, mode)?;
    let item: ItemStruct = syn::parse2(item)?;
    let ident = &item.ident;
    let vis = &item.vis;
    let attrs = &item.attrs;
    if !item.generics.params.is_empty() {
        return err(item.generics.span(), "model declarations cannot be generic");
    }

    // The model type field handles point to.
    let (model_ty, model_name, kind) = match mode {
        Mode::New => {
            let name = args.name.clone();
            (
                quote!(#ident),
                quote!(#name),
                args.kind
                    .clone()
                    .unwrap_or_else(|| quote!(#rt::ModelKind::Regular)),
            )
        }
        Mode::Extend => {
            let target = args
                .target
                .clone()
                .ok_or_else(|| syn::Error::new(Span::call_site(), "expected #[extend(Model)]"))?;
            (
                quote!(#target),
                quote!(<#target as #rt::Model>::MODEL_NAME),
                args.kind
                    .clone()
                    .unwrap_or_else(|| quote!(<#target as #rt::Model>::KIND)),
            )
        }
    };

    let mut decls = Vec::new();
    let mut consts = Vec::new();
    let mut consts_names: Vec<syn::Ident> = Vec::new();
    let mut checks = Vec::new();
    let mut field_descs: Vec<serde_json::Value> = Vec::new();
    match &item.fields {
        Fields::Named(named) => {
            for f in &named.named {
                let Some(fident) = &f.ident else { continue };
                let fname = fident.to_string().trim_start_matches("r#").to_owned();
                let attr = f
                    .attrs
                    .iter()
                    .find(|a| a.path().is_ident("field"))
                    .ok_or_else(|| {
                        syn::Error::new(f.span(), "every field needs #[field(kind, ...)]")
                    })?;
                let out = parse_field(&fname, attr)?;
                // `parse_field` has checked the shape: the kind first, then the attributes
                let metas = parse_metas(attr.parse_args::<TokenStream>()?)?;
                field_descs.push(serde_json::json!({
                    "name": fname,
                    "kind": metas.first().map(crate::attrs::key).unwrap_or_default(),
                    "attrs": crate::descriptor::metas_json(metas.iter().skip(1)),
                }));
                decls.push(out.decl);
                let ty = &f.ty;
                let cname = const_name(&fname);
                consts_names.push(cname.clone());
                let docs: Vec<_> = f
                    .attrs
                    .iter()
                    .filter(|a| a.path().is_ident("doc"))
                    .collect();
                let fvis = &f.vis;
                consts.push(quote! {
                    #(#docs)*
                    #fvis const #cname: #rt::Field<#model_ty, #ty> = #rt::Field::new(#fname, #rt::Token::__macro());
                });
                let marker = out.marker;
                checks.push(quote_spanned! {ty.span()=>
                    __erp_check::<#ty, #marker>();
                });
            }
        }
        Fields::Unit => {}
        Fields::Unnamed(u) => return err(u.span(), "use named fields"),
    }

    for (name, ident) in &args.keys {
        if consts_names.iter().any(|c| c == ident) {
            return err(
                name.span(),
                format!(
                    "key '{}' gives the associated constant {ident}, which a field handle already \
                     uses; rename the key",
                    name.value()
                ),
            );
        }
        consts_names.push(ident.clone());
        let doc = format!("Business key `{}` (for `Upsert::on`).", name.value());
        consts.push(quote! {
            #[doc = #doc]
            #vis const #ident: #rt::KeyRef<#model_ty> = #rt::KeyRef::new(#name, #rt::Token::__macro());
        });
    }

    let addon = addon_tokens(args.addon.clone());
    let mut inherit: Vec<TokenStream> = args
        .inherit
        .iter()
        .map(|p| quote!(<#p as #rt::Model>::MODEL_NAME))
        .collect();
    if mode == Mode::Extend {
        inherit.insert(0, model_name.clone());
    }
    let mut calls = Vec::new();
    calls.push(quote!(.inherit(const { &[#(#inherit),*] })));
    calls.push(quote!(.kind(#kind)));
    if let Some(d) = &args.description {
        calls.push(quote!(.description(#d)));
    }
    if let Some(t) = &args.table {
        calls.push(quote!(.table(#t)));
    }
    if let Some(on) = args.audit_reads {
        calls.push(quote!(.audit_reads(#on)));
    }
    if let Some(on) = args.eventful {
        calls.push(quote!(.eventful(#on)));
    }
    if let Some(l) = args.log_access {
        calls.push(quote!(.log_access(#l)));
    }
    if let Some(r) = &args.rec_name {
        calls.push(quote!(.rec_name(#r)));
    }
    if let Some(names) = &args.rec_names_search {
        calls.push(quote!(.rec_names_search(const { &[#(#names),*] })));
    }
    if let Some(a) = &args.active_name {
        calls.push(quote!(.active_name(#a)));
    }
    if let Some(names) = &args.field_parameters {
        calls.push(quote!(.field_parameters(const { &[#(#names),*] })));
    }
    if let Some(names) = &args.owner_only_stored_fields {
        calls.push(quote!(.owner_only_stored_fields(const { &[#(#names),*] })));
    }
    if let Some(o) = &args.order {
        calls.push(quote!(.order(#o)));
    }
    if !args.inherits.is_empty() {
        let pairs = args.inherits.iter().map(|(m, f)| quote!((#m, #f)));
        calls.push(quote!(.inherits(const { &[#(#pairs),*] })));
    }
    if let Some(on) = args.check_company_auto {
        calls.push(quote!(.check_company_auto(#on)));
    }
    if let Some(m) = &args.company_match {
        calls.push(quote!(.company_match(#m)));
    }
    if let Some(access) = &args.company_access {
        calls.push(quote!(.company_access(#access)));
    }
    if let Some(on) = args.allow_sudo_commands {
        calls.push(quote!(.allow_sudo_commands(#on)));
    }
    if let Some(on) = args.inherits_access {
        calls.push(quote!(.inherits_access(#on)));
    }
    if let Some(on) = args.parent_store {
        calls.push(quote!(.parent_store(#on)));
    }
    if let Some(ff) = args.fillfactor {
        calls.push(quote!(.fillfactor(#ff)));
    }
    if let Some(n) = args.transient_max_age_seconds {
        calls.push(quote!(.transient_max_age_seconds(#n)));
    }
    if let Some(n) = args.transient_max_count {
        calls.push(quote!(.transient_max_count(#n)));
    }
    if let Some(seq) = &args.id_sequence {
        calls.push(id_sequence_tokens(seq));
    }
    if let Some(p) = &args.parent_name {
        calls.push(quote!(.parent_name(#p)));
    }
    if let Some(partition) = &args.partition {
        calls.push(quote!(.partition(#partition)));
    }
    if let Some(on) = args.row_version {
        calls.push(quote!(.row_version(#on)));
    }
    let objects = &args.table_objects;
    calls.push(quote!(.fields(const { &[#(#decls),*] })));
    calls.push(quote!(.table_objects(const { &[#(#objects),*] })));
    if !args.extensions.is_empty() {
        let extensions = &args.extensions;
        calls.push(quote!(.extensions(const { &[#(#extensions),*] })));
    }

    let static_name = format_ident!("__ERP_MODEL_DECL_{}", ident.to_string().to_uppercase());
    let model_impl = match mode {
        Mode::New => {
            let parents = &args.inherit;
            quote! {
                impl #rt::Model for #ident {
                    const MODEL_NAME: &'static str = #model_name;
                    const KIND: #rt::ModelKind = #kind;
                }
                impl #rt::Inherits<#ident> for #ident {
                    const __TOKEN: #rt::Token = #rt::Token::__macro();
                }
                #(impl #rt::Inherits<#parents> for #ident {
                    const __TOKEN: #rt::Token = #rt::Token::__macro();
                })*
            }
        }
        Mode::Extend => quote! {},
    };
    let note = match mode {
        Mode::New => {
            "\n\nModel declaration type generated by `#[model]`: it has no values and is \
                      never a record. Records are `RecordSet<Self>`; fields are the associated \
                      constants."
        }
        Mode::Extend => {
            "\n\nModel extension type generated by `#[extend]`: it has no values. \
                         Its associated constants are field handles of the extended model."
        }
    };

    // The backend-neutral description (/003). `extend`'s first argument is the
    // target type, the rest are options like `#[model]`'s.
    let (target_text, option_metas) = match mode {
        Mode::New => (None, arg_metas.iter().collect::<Vec<_>>()),
        Mode::Extend => (
            arg_metas.first().map(crate::descriptor::text),
            arg_metas.iter().skip(1).collect(),
        ),
    };
    let detail = serde_json::json!({
        "target": target_text,
        "args": crate::descriptor::metas_json(option_metas),
        "fields": field_descs,
    });
    let register = crate::descriptor::register(
        &addon,
        match mode {
            Mode::New => "Model",
            Mode::Extend => "Extend",
        },
        &model_name,
        &ident.to_string(),
        &detail,
    );

    Ok(quote! {
        #(#attrs)*
        #[doc = #note]
        #vis enum #ident {}

        #model_impl

        #[allow(dead_code)]
        impl #ident {
            #(#consts)*
        }

        const _: () = {
            #[allow(dead_code)]
            fn __erp_check<T: #rt::ValueFor<K>, K>() {}
            #[allow(dead_code)]
            fn __erp_checks() {
                #(#checks)*
            }
        };

        // the registry's declaration is native-only: a component registers the description below
        #[cfg(not(target_family = "wasm"))]
        const _: () = {
            #[#rt::linkme::distributed_slice(#rt::MODEL_DECLS)]
            #[linkme(crate = #rt::linkme)]
            static #static_name: #rt::ModelDecl = #rt::ModelDecl::new(#addon, #model_name)
                #(#calls)*
                .at(::core::file!(), ::core::line!());
        };

        #register
    })
}

pub(crate) fn expand_addon(input: TokenStream) -> syn::Result<TokenStream> {
    let rt = rt();
    let metas = parse_metas(input)?;
    let mut name = None;
    let mut depends = Vec::new();
    let mut exact_only = None;
    for meta in &metas {
        match key(meta).as_str() {
            "name" => name = Some(lit_str(value(meta)?)?),
            "exact_only" => exact_only = Some(lit_bool(value(meta)?)?),
            "depends" => {
                for e in array(value(meta)?)? {
                    depends.push(lit_str(e)?);
                }
            }
            other => return err(meta.span(), format!("unknown addon parameter `{other}`")),
        }
    }
    let Some(name) = name else {
        return err(Span::call_site(), "addon!() needs name = \"...\"");
    };
    if !depends.iter().any(|d| d.value() == "base") && name.value() != "base" {
        depends.insert(0, syn::LitStr::new("base", Span::call_site()));
    }
    let mandatory = name.value().starts_with("account")
        || name.value().starts_with("stock")
        || matches!(name.value().as_str(), "product" | "uom");
    if mandatory && exact_only == Some(false) {
        return err(name.span(), "exact_only cannot be disabled for this addon");
    }
    let exact_only = exact_only.unwrap_or(mandatory);
    let detail = serde_json::json!({ "depends": crate::descriptor::lits_json(&depends), "exact_only": exact_only });
    let register =
        crate::descriptor::register(&quote!(#name), "Addon", &quote!(""), &name.value(), &detail);
    Ok(quote! {
        /// This addon's descriptor (name and dependencies), named by link roots.
        pub static ADDON: #rt::AddonDescriptor = #rt::AddonDescriptor {
            name: #name,
            depends: &[#(#depends),*], exact_only: #exact_only };
        #[doc(hidden)]
        pub const __ERP_ADDON: &str = #name;
        #register
    })
}

/// The `CompanyMatch` variant a `company_match = "…"` spelling stands for. The spellings are
/// those of `CompanyMatch::parse` (a test ties the two).
fn company_match_variant(spelling: &str) -> Option<&'static str> {
    match spelling {
        "exact" => Some("Exact"),
        "parent_of" => Some("ParentOf"),
        "any_of" => Some("AnyOf"),
        "parent_of_any" => Some("ParentOfAny"),
        _ => None,
    }
}

#[cfg(test)]
mod company_match_tests {
    use super::company_match_variant;

    /// Review T1: the macro's spellings are exactly `CompanyMatch::parse`'s, variant for variant.
    #[test]
    fn macro_spellings_are_the_registry_spellings() {
        for name in erp_registry::CompanyMatch::NAMES {
            let parsed = erp_registry::CompanyMatch::parse(name).unwrap();
            assert_eq!(
                company_match_variant(name),
                Some(format!("{parsed:?}").as_str()),
                "{name}"
            );
        }
        assert_eq!(company_match_variant("Exact"), None);
        assert_eq!(company_match_variant(""), None);
    }

    /// `company_access` may be given once per declaration, on `#[model]` and `#[extend]`.
    #[test]
    fn company_access_is_given_once() {
        use super::{Mode, parse_model_args};
        let twice = quote::quote!(
            name = "x.doc",
            company_access = parent(edge = "doc_id"),
            company_access = parent(edge = "doc_id")
        );
        assert_eq!(
            parse_model_args(twice, Mode::New)
                .err()
                .map(|e| e.to_string()),
            Some("company_access is given twice".to_owned())
        );
        let ext = quote::quote!(XDoc, company_access = unrestricted(reason = "shared"));
        let args = parse_model_args(ext, Mode::Extend).ok().unwrap();
        assert!(args.company_access.is_some());
        let bad = quote::quote!(name = "x.doc", company_access = "direct");
        assert_eq!(
            parse_model_args(bad, Mode::New)
                .err()
                .map(|e| e.to_string()),
            Some(crate::company_access::USAGE.to_owned())
        );
    }
}

#[cfg(test)]
mod id_sequence_tests {
    #![allow(clippy::panic, reason = "tests")]

    use quote::quote;

    use super::{Mode, check_sequence_name, parse_model_args};

    fn parse_error(attr: proc_macro2::TokenStream) -> String {
        match parse_model_args(attr, Mode::New) {
            Ok(_) => panic!("accepted"),
            Err(e) => e.to_string(),
        }
    }

    /// Review C-T4: a second `id_sequence` is refused (it used to win silently), and a literal
    /// that is not a PostgreSQL identifier of at most 63 bytes fails at compile time.
    #[test]
    fn id_sequence_is_given_once_and_checked() {
        assert_eq!(
            parse_error(quote!(
                name = "x.a",
                id_sequence = "a_seq",
                id_sequence = "b_seq"
            )),
            "`id_sequence` is given twice"
        );
        assert_eq!(
            parse_error(quote!(name = "x.a", id_sequence = "1seq")),
            "id_sequence \"1seq\" is not a PostgreSQL identifier: ASCII letters, digits, '_' and \
             '$', starting with a letter or '_'"
        );
        let long = "s".repeat(64);
        assert_eq!(
            parse_error(quote!(name = "x.a", id_sequence = #long)),
            format!(
                "id_sequence \"{long}\" is 64 bytes long; PostgreSQL identifiers have at most 63"
            )
        );
        assert_eq!(
            parse_error(quote!(name = "x.a", id_sequence = 3)),
            "id_sequence is a string literal or the path of a `&str` constant"
        );
        assert!(parse_model_args(quote!(name = "x.a", id_sequence = SEQ), Mode::New).is_ok());
        assert!(parse_model_args(quote!(name = "x.a", id_sequence = "a_seq$1"), Mode::New).is_ok());
        assert_eq!(check_sequence_name(&"s".repeat(63)), Ok(()));
        for bad in ["", "a-b", "é", "$a", "a b"] {
            assert!(check_sequence_name(bad).is_err(), "{bad:?}");
        }
        // the macro's rule is the registry's const rule
        for name in [
            "a",
            "_a",
            "a$",
            "A1",
            "",
            "1",
            "$",
            "a-b",
            &"s".repeat(63),
            &"s".repeat(64),
        ] {
            assert_eq!(
                check_sequence_name(name).is_ok(),
                erp_registry::ModelDecl::valid_id_sequence(name),
                "{name:?}"
            );
        }
    }

    #[test]
    fn follow_delete_entries() {
        assert!(
            parse_model_args(
                quote!(
                    name = "x.a",
                    follow_deletes = [
                        ("_keys", cache = "actions", key_table = KEYS),
                        ("_groups", cache = CACHE, relation = "group_ids")
                    ]
                ),
                Mode::New
            )
            .is_ok()
        );
        assert_eq!(
            parse_error(quote!(
                name = "x.a",
                follow_deletes = [("_keys", key_table = "t")]
            )),
            "a follow-delete trigger needs `cache = \"...\"`"
        );
        assert_eq!(
            parse_error(quote!(
                name = "x.a",
                follow_deletes = [("_keys", cache = "a", cache = "b")]
            )),
            "`cache` given twice"
        );
    }
}

#[cfg(test)]
mod native_field_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
    use super::*;

    #[test]
    fn field_collation_parameter_requires_a_string() {
        let invalid: syn::Attribute = syn::parse_quote!(#[field(char, collation = 42)]);
        let error = parse_field("code", &invalid)
            .err()
            .expect("non-string collation refused");
        assert_eq!(error.to_string(), "expected a string literal");
    }

    #[test]
    fn native_fields_emit_exact_kinds_without_char_substitution() {
        for (attribute, kind) in [
            (syn::parse_quote!(#[field(uuid)]), "Uuid"),
            (syn::parse_quote!(#[field(xid8)]), "Xid8"),
            (syn::parse_quote!(#[field(timestamptz)]), "TimestampTz"),
        ] {
            let parsed = parse_field("native_value", &attribute).unwrap();
            assert!(
                parsed
                    .decl
                    .to_string()
                    .contains(&format!("KindDecl :: {kind}"))
            );
            assert_eq!(
                parsed.marker.to_string(),
                format!(":: erp_sdk :: __private :: kinds :: {kind}")
            );
        }
        let invalid: syn::Attribute = syn::parse_quote!(#[field(uuid, size = 12)]);
        let error = parse_field("native_value", &invalid)
            .err()
            .expect("size refused");
        assert_eq!(error.to_string(), "`size` does not apply to uuid fields");
    }
}

#[cfg(test)]
mod eventful_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
    use super::*;

    #[test]
    fn eventful_attribute_is_a_boolean_on_models_and_extensions() {
        assert_eq!(
            parse_model_args(quote!(name = "test.eventful", eventful = true), Mode::New)
                .unwrap()
                .eventful,
            Some(true)
        );
        assert_eq!(
            parse_model_args(quote!(TestModel, eventful = false), Mode::Extend)
                .unwrap()
                .eventful,
            Some(false)
        );
        let expanded = expand(
            quote!(name = "test.eventful", eventful = true),
            quote!(
                pub struct Eventful {}
            ),
            Mode::New,
        )
        .unwrap();
        assert!(expanded.to_string().contains(". eventful (true)"));
        let error = parse_model_args(quote!(name = "test.bad", eventful = "true"), Mode::New)
            .err()
            .expect("a string is not a boolean");
        assert_eq!(error.to_string(), "expected `true` or `false`");
        let duplicate = parse_model_args(
            quote!(name = "test.bad", eventful = true, eventful = false),
            Mode::New,
        )
        .err()
        .expect("duplicate opt-in is ambiguous input");
        assert_eq!(duplicate.to_string(), "`eventful` given twice");
        for (attr, key) in [
            (quote!(name = "test.bad", auto = false), "auto"),
            (quote!(name = "test.bad", view = true), "view"),
        ] {
            let error = parse_model_args(attr, Mode::New)
                .err()
                .expect("tableless/view attributes are not implemented");
            assert_eq!(
                error.to_string(),
                format!("unknown model parameter `{key}`")
            );
        }
    }
}

#[cfg(test)]
mod exact_numeric_tests {
    use super::expand_addon;
    use quote::quote;

    #[test]
    fn exact_only_addon_macro_is_strict_and_described() {
        assert!(
            expand_addon(quote!(name = "ordinary", exact_only = true))
                .unwrap()
                .to_string()
                .contains("exact_only")
        );
        for tokens in [
            quote!(name = "uom", exact_only = false),
            quote!(name = "stock_extra", exact_only = false),
        ] {
            assert_eq!(
                expand_addon(tokens).unwrap_err().to_string(),
                "exact_only cannot be disabled for this addon"
            );
        }
        assert_eq!(
            expand_addon(quote!(name = "ordinary", exact_only = "true"))
                .unwrap_err()
                .to_string(),
            "expected `true` or `false`"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
mod exact_numeric_policy_consistency_tests {
    use quote::quote;

    #[test]
    fn macro_and_registry_share_mandatory_name_semantics() {
        for name in [
            "account",
            "accountant",
            "stock",
            "stocktake",
            "product",
            "uom",
            "product_expiry",
            "sale_stock",
        ] {
            let name_literal = syn::LitStr::new(name, proc_macro2::Span::call_site());
            let expanded: syn::File =
                syn::parse2(super::expand_addon(quote!(name = #name_literal)).unwrap()).unwrap();
            let addon = expanded
                .items
                .iter()
                .find_map(|item| match item {
                    syn::Item::Static(item) if item.ident == "ADDON" => Some(item),
                    _ => None,
                })
                .unwrap();
            let syn::Expr::Struct(value) = &*addon.expr else {
                panic!("expected addon descriptor");
            };
            let policy = value
                .fields
                .iter()
                .find(|field| {
                    field.member
                        == syn::Member::Named(syn::Ident::new(
                            "exact_only",
                            proc_macro2::Span::call_site(),
                        ))
                })
                .unwrap();
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Bool(policy),
                ..
            }) = &policy.expr
            else {
                panic!("expected boolean policy");
            };
            let expected = erp_registry::AddonDescriptor::requires_exact(name);
            assert_eq!(policy.value, expected, "{name}");
            let disabled = super::expand_addon(quote!(name = #name_literal, exact_only = false));
            if expected {
                assert_eq!(
                    disabled.unwrap_err().to_string(),
                    "exact_only cannot be disabled for this addon"
                );
            } else {
                disabled.unwrap();
            }
        }
    }
}

#[cfg(test)]
mod audit_reads_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
    use super::*;
    #[test]
    fn audit_reads_attribute_is_a_boolean_on_models_and_extensions() {
        assert_eq!(
            parse_model_args(
                quote!(name = "test.audit_reads", audit_reads = true),
                Mode::New
            )
            .unwrap()
            .audit_reads,
            Some(true)
        );
        assert_eq!(
            parse_model_args(quote!(TestModel, audit_reads = false), Mode::Extend)
                .unwrap()
                .audit_reads,
            Some(false)
        );
        let expanded = expand(
            quote!(name = "test.audit_reads", audit_reads = true),
            quote!(
                pub struct Audited {}
            ),
            Mode::New,
        )
        .unwrap();
        assert!(expanded.to_string().contains(". audit_reads (true)"));
        let error = parse_model_args(quote!(name = "test.bad", audit_reads = "true"), Mode::New)
            .err()
            .expect("a string is not a boolean");
        assert_eq!(error.to_string(), "expected `true` or `false`");
        let duplicate = parse_model_args(
            quote!(name = "test.bad", audit_reads = true, audit_reads = false),
            Mode::New,
        )
        .err()
        .expect("duplicate opt-in is ambiguous input");
        assert_eq!(duplicate.to_string(), "`audit_reads` given twice");
    }
}

#[cfg(test)]
mod owner_only_stored_fields_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
    use super::*;
    #[test]
    fn owner_only_stored_fields_attribute_lists_co_owners() {
        let args = parse_model_args(
            quote!(
                name = "test.owned",
                owner_only_stored_fields = ["co_a", "co_b"]
            ),
            Mode::New,
        )
        .unwrap();
        let names: Vec<String> = args
            .owner_only_stored_fields
            .unwrap()
            .iter()
            .map(syn::LitStr::value)
            .collect();
        assert_eq!(names, ["co_a", "co_b"]);
        let expanded = expand(
            quote!(name = "test.owned", owner_only_stored_fields = []),
            quote!(
                pub struct Owned {}
            ),
            Mode::New,
        )
        .unwrap();
        assert!(
            expanded
                .to_string()
                .contains(". owner_only_stored_fields (const { & [] })")
        );
        let duplicate = parse_model_args(
            quote!(
                name = "test.bad",
                owner_only_stored_fields = [],
                owner_only_stored_fields = []
            ),
            Mode::New,
        )
        .err()
        .expect("a second policy is ambiguous input");
        assert_eq!(
            duplicate.to_string(),
            "`owner_only_stored_fields` given twice"
        );
        let extension = parse_model_args(
            quote!(TestModel, owner_only_stored_fields = ["co_a"]),
            Mode::Extend,
        )
        .err()
        .expect("an extension cannot declare the policy");
        assert_eq!(
            extension.to_string(),
            "`owner_only_stored_fields` is declared by the model's owner in #[model], not in \
             #[extend]"
        );
    }
}

#[cfg(test)]
mod selection_cleanup_tests {
    #![allow(clippy::unwrap_used, reason = "test assertions")]
    use super::*;
    #[test]
    fn selection_add_declares_cleanup_policy() {
        let expanded=expand(quote!(Base, addon = "stock"),quote! {
            struct Rules {
                #[field(selection, selection_add = [("weight", "Weight")], ondelete = [("weight", "set default")])]
                pub r#type: Option<Arc<str>>,
            }
        },Mode::Extend).unwrap().to_string();
        assert!(expanded.contains("SelectionOnDeleteDecl :: SetDefault"));
        let error=expand(quote!(Base, addon = "stock"),quote! {
            struct Rules {
                #[field(selection, selection_add = [("weight", "Weight")], ondelete = [("weight", "ignore")])]
                pub r#type: Option<Arc<str>>,
            }
        },Mode::Extend).unwrap_err();
        assert_eq!(
            error.to_string(),
            "selection ondelete must be 'set null', 'cascade' or 'set default'"
        );
    }
}
