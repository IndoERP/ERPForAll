//! `model_ref!`: name a model another addon declares, optionally with typed handles for the
//! fields it declares ("Extending a data-only addon's model").

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Attribute, Ident, LitStr, Token, Type, Visibility, braced};

use crate::model::{addon_tokens, rt};

struct RefField {
    attrs: Vec<Attribute>,
    name: Ident,
    ty: Type,
}

impl Parse for RefField {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let name: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let ty: Type = input.parse()?;
        Ok(Self { attrs, name, ty })
    }
}

struct ModelRef {
    attrs: Vec<Attribute>,
    vis: Visibility,
    name: Ident,
    model: LitStr,
    fields: Vec<RefField>,
}

impl Parse for ModelRef {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let vis: Visibility = input.parse()?;
        let name: Ident = input.parse()?;
        input.parse::<Token![=]>()?;
        let model: LitStr = input.parse()?;
        let mut fields = Vec::new();
        if input.peek(syn::token::Brace) {
            let content;
            braced!(content in input);
            let list = Punctuated::<RefField, Token![,]>::parse_terminated(&content)?;
            fields = list.into_iter().collect();
        }
        if !input.is_empty() {
            return Err(input.error("unexpected tokens after the model reference"));
        }
        Ok(Self {
            attrs,
            vis,
            name,
            model,
            fields,
        })
    }
}

pub(crate) fn expand(input: TokenStream) -> syn::Result<TokenStream> {
    let parsed: ModelRef = syn::parse2(input)?;
    let ModelRef {
        attrs,
        vis,
        name,
        model,
        fields,
    } = parsed;
    let rt = rt();
    let mut seen = std::collections::BTreeSet::new();
    let mut consts = Vec::new();
    let mut shapes = Vec::new();
    for f in &fields {
        let fname = f.name.to_string();
        if !seen.insert(fname.clone()) {
            return Err(syn::Error::new(
                f.name.span(),
                format!(
                    "field `{fname}` is named twice in the reference to {}",
                    model.value()
                ),
            ));
        }
        let const_name = format_ident!("{}", fname.to_uppercase(), span = f.name.span());
        let (attrs, ty) = (&f.attrs, &f.ty);
        consts.push(quote! {
            #(#attrs)*
            #vis const #const_name: #rt::Field<#name, #ty> =
                #rt::Field::new(#fname, #rt::Token::__macro());
        });
        shapes.push(quote! {
            (
                #fname,
                <#ty as #rt::RefType>::field_kinds(),
                <#ty as #rt::RefType>::comodel(),
            )
        });
    }
    let handles = if consts.is_empty() {
        quote!()
    } else {
        quote! { impl #name { #(#consts)* } }
    };
    let register = if shapes.is_empty() {
        quote!()
    } else {
        let addon = addon_tokens(None);
        quote! {
            ::erp_sdk::__register_contribution!(::erp_sdk::__private::Contribution {
                addon: #addon,
                kind: ::erp_sdk::__private::ContributionKind::ModelRef,
                model: #model,
                name: stringify!(#name),
                detail: "",
                detail_fn: ::core::option::Option::Some({
                    fn detail() -> ::std::string::String {
                        #rt::__model_ref_detail(&[#(#shapes),*])
                    }
                    detail
                }),
            });
        }
    };
    Ok(quote! {
        #(#attrs)*
        #vis enum #name {}
        impl #rt::Model for #name {
            const MODEL_NAME: &'static str = #model;
            const KIND: #rt::ModelKind = #rt::ModelKind::Regular;
        }
        impl #rt::Inherits<#name> for #name {
            const __TOKEN: #rt::Token = #rt::Token::__macro();
        }
        #handles
        #register
    })
}
