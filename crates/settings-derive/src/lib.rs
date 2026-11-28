//! `#[derive(Knobs)]` for the dashboard settings tree.
//!
//! Applied to an effective settings struct, it generates the sparse
//! `*Overrides` mirror persisted in `settings.ron` plus an implementation of
//! `crate::settings::knob::Knobs`: resolve, patch-merge, form parsing,
//! constant range checks (and dropping out-of-range overrides), and the
//! `KnobMeta` table walk. The runtime half
//! lives in `twitch-1337-core` (`settings/knob.rs`); generated code names it
//! through `crate::settings::knob`, so the derive only works inside that crate.
//!
//! Struct attributes, `#[knobs(...)]`:
//! - `overrides = Ident`: name of the generated overrides struct
//!   (default `<Struct>Overrides`).
//! - `presence`: the struct sits behind an `Option` in its parent; the
//!   overrides get an `enabled: Option<bool>` and `PresenceKnobs` is derived.
//!
//! Field attributes, `#[knob(...)]` (none = plain knob):
//! - `nested`: the field's type derives `Knobs` itself.
//! - `presence`: the field is `Option<T>` with `T: PresenceKnobs`.
//! - `required_profile`: `String` profile pointer; blank falls back to the default.
//! - `schema_version`: the `u32` file version, never a knob.
//! - `form = false`: not posted by the settings form.
//! - `restart`: a change only takes effect after a bot restart.
//! - `min = EXPR`, `max = EXPR`: inclusive range check and form bounds.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    Data, DeriveInput, Expr, ExprLit, Fields, GenericArgument, Ident, Lit, LitBool, LitStr,
    PathArguments, Token, Type, parse_macro_input,
};

#[proc_macro_derive(Knobs, attributes(knob, knobs))]
pub fn derive_knobs(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

enum Kind {
    Plain,
    Nested,
    /// Holds the `T` of the field's `Option<T>`.
    Presence(Box<Type>),
    RequiredProfile,
    SchemaVersion,
}

struct Field {
    ident: Ident,
    ty: Type,
    kind: Kind,
    form: bool,
    restart: bool,
    min: Option<Expr>,
    max: Option<Expr>,
    aliases: Vec<LitStr>,
}

struct StructAttrs {
    overrides: Ident,
    presence: bool,
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "#[derive(Knobs)] does not support generics",
        ));
    }
    let attrs = struct_attrs(input)?;
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            name,
            "#[derive(Knobs)] only supports structs",
        ));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new_spanned(
            name,
            "#[derive(Knobs)] needs named fields",
        ));
    };
    let fields = named
        .named
        .iter()
        .map(field_spec)
        .collect::<syn::Result<Vec<_>>>()?;

    let rt = quote!(crate::settings::knob);
    let overrides = &attrs.overrides;

    let mut ov_fields = Vec::new();
    let mut ov_defaults = Vec::new();
    let mut resolve = Vec::new();
    let mut merge = Vec::new();
    let mut from_form = Vec::new();
    let mut ranges = Vec::new();
    let mut drops = Vec::new();
    let mut visit = Vec::new();

    if attrs.presence {
        ov_fields.push(quote! {
            #[serde(default)]
            pub enabled: ::core::option::Option<bool>,
        });
        ov_defaults.push(quote!(enabled: ::core::option::Option::None,));
        merge.push(quote! {
            if patch.enabled.is_some() {
                into.enabled = patch.enabled;
            }
        });
        from_form.push(quote! {
            enabled: #rt::parse_bool(form, prefix, &#rt::join(prefix, "enabled")),
        });
    }

    for f in &fields {
        let ident = &f.ident;
        let ty = &f.ty;
        let key = ident.to_string();
        let aliases = &f.aliases;
        let serde_attr = quote!(#[serde(default #(, alias = #aliases)*)]);
        let form = f.form;
        let restart = f.restart;
        let bound = |e: &Option<Expr>| match e {
            Some(e) => quote!(::core::option::Option::Some(#e)),
            None => quote!(::core::option::Option::None),
        };
        let bound_str = |e: &Option<Expr>| match e {
            Some(e) => quote!(::core::option::Option::Some((#e).to_string())),
            None => quote!(::core::option::Option::None),
        };
        let push_meta = {
            let min = bound_str(&f.min);
            let max = bound_str(&f.max);
            quote! {
                out.push(#rt::KnobMeta {
                    path: #rt::join(prefix, #key),
                    form: #form,
                    restart: #restart,
                    presence: false,
                    min: #min,
                    max: #max,
                });
            }
        };

        match &f.kind {
            Kind::Plain | Kind::RequiredProfile => {
                let required = matches!(f.kind, Kind::RequiredProfile);
                let ov_ty = if required {
                    quote!(::core::option::Option<::core::option::Option<::std::string::String>>)
                } else {
                    quote!(::core::option::Option<#ty>)
                };
                ov_fields.push(quote! {
                    #serde_attr
                    pub #ident: #ov_ty,
                });
                ov_defaults.push(quote!(#ident: ::core::option::Option::None,));
                resolve.push(if required {
                    quote! {
                        #ident: match &o.#ident {
                            ::core::option::Option::Some(::core::option::Option::Some(n))
                                if !n.trim().is_empty() => n.clone(),
                            _ => d.#ident.clone(),
                        },
                    }
                } else {
                    quote!(#ident: o.#ident.clone().unwrap_or_else(|| d.#ident.clone()),)
                });
                merge.push(quote! {
                    if patch.#ident.is_some() {
                        into.#ident.clone_from(&patch.#ident);
                    }
                });
                let parse = if !form {
                    quote!(::core::option::Option::None)
                } else if required {
                    quote!(#rt::parse_required_profile(form, &#rt::join(prefix, #key), errs))
                } else if is_bool(ty) {
                    quote!(#rt::parse_bool(form, prefix, &#rt::join(prefix, #key)))
                } else {
                    quote!(#rt::parse_field::<#ty>(form, &#rt::join(prefix, #key), errs))
                };
                from_form.push(quote!(#ident: #parse,));
                if f.min.is_some() || f.max.is_some() {
                    let min = bound(&f.min);
                    let max = bound(&f.max);
                    ranges.push(quote! {
                        #rt::check_range::<#ty>(prefix, #key, self.#ident, #min, #max, errs);
                    });
                    drops.push(quote! {
                        if let ::core::option::Option::Some(v) = o.#ident {
                            let before = errs.len();
                            #rt::check_range::<#ty>(prefix, #key, v, #min, #max, errs);
                            if errs.len() > before {
                                o.#ident = ::core::option::Option::None;
                            }
                        }
                    });
                }
                visit.push(push_meta);
            }
            Kind::Nested => {
                ov_fields.push(quote! {
                    #serde_attr
                    pub #ident: <#ty as #rt::Knobs>::Overrides,
                });
                ov_defaults.push(quote!(#ident: ::core::default::Default::default(),));
                resolve.push(
                    quote!(#ident: <#ty as #rt::Knobs>::resolve_knobs(&d.#ident, &o.#ident),),
                );
                merge.push(
                    quote!(<#ty as #rt::Knobs>::merge_knobs(&mut into.#ident, &patch.#ident);),
                );
                from_form.push(quote! {
                    #ident: <#ty as #rt::Knobs>::knobs_from_form(&#rt::join(prefix, #key), form, errs),
                });
                ranges.push(quote! {
                    #rt::Knobs::check_knob_ranges(&self.#ident, &#rt::join(prefix, #key), errs);
                });
                drops.push(quote! {
                    <#ty as #rt::Knobs>::drop_out_of_range(&mut o.#ident, &#rt::join(prefix, #key), errs);
                });
                visit
                    .push(quote!(<#ty as #rt::Knobs>::visit_knobs(&#rt::join(prefix, #key), out);));
            }
            Kind::Presence(inner) => {
                ov_fields.push(quote! {
                    #serde_attr
                    pub #ident: <#inner as #rt::Knobs>::Overrides,
                });
                ov_defaults.push(quote!(#ident: ::core::default::Default::default(),));
                resolve.push(quote! {
                    #ident: <#inner as #rt::PresenceKnobs>::resolve_presence(d.#ident.as_ref(), &o.#ident),
                });
                merge.push(
                    quote!(<#inner as #rt::Knobs>::merge_knobs(&mut into.#ident, &patch.#ident);),
                );
                from_form.push(quote! {
                    #ident: <#inner as #rt::Knobs>::knobs_from_form(&#rt::join(prefix, #key), form, errs),
                });
                ranges.push(quote! {
                    if let ::core::option::Option::Some(v) = &self.#ident {
                        #rt::Knobs::check_knob_ranges(v, &#rt::join(prefix, #key), errs);
                    }
                });
                drops.push(quote! {
                    <#inner as #rt::Knobs>::drop_out_of_range(&mut o.#ident, &#rt::join(prefix, #key), errs);
                });
                visit.push(quote! {
                    let path = #rt::join(prefix, #key);
                    out.push(#rt::KnobMeta {
                        path: #rt::join(&path, "enabled"),
                        form: true,
                        restart: #restart,
                        presence: true,
                        min: ::core::option::Option::None,
                        max: ::core::option::Option::None,
                    });
                    <#inner as #rt::Knobs>::visit_knobs(&path, out);
                });
            }
            Kind::SchemaVersion => {
                ov_fields.push(quote! {
                    #[serde(default = "crate::settings::default_schema_version")]
                    pub #ident: #ty,
                });
                ov_defaults.push(quote!(#ident: crate::settings::default_schema_version(),));
                resolve.push(quote!(#ident: d.#ident,));
                from_form.push(quote!(#ident: crate::settings::default_schema_version(),));
            }
        }
    }

    let doc = format!(
        "Sparse overrides for [`{name}`], persisted in `settings.ron`. `None` falls \
         through to the default. Generated by `#[derive(Knobs)]`."
    );

    let presence_impl = attrs.presence.then(|| {
        quote! {
            #[automatically_derived]
            impl #rt::PresenceKnobs for #name {
                fn resolve_presence(
                    d: ::core::option::Option<&Self>,
                    o: &#overrides,
                ) -> ::core::option::Option<Self> {
                    let enabled = o.enabled.unwrap_or_else(|| d.is_some());
                    if !enabled {
                        return ::core::option::Option::None;
                    }
                    let base = d.cloned().unwrap_or_default();
                    ::core::option::Option::Some(<Self as #rt::Knobs>::resolve_knobs(&base, o))
                }
            }
        }
    });

    Ok(quote! {
        #[doc = #doc]
        #[derive(Debug, Clone, PartialEq, ::serde::Serialize, ::serde::Deserialize)]
        pub struct #overrides {
            #(#ov_fields)*
        }

        #[automatically_derived]
        impl ::core::default::Default for #overrides {
            fn default() -> Self {
                Self { #(#ov_defaults)* }
            }
        }

        #[automatically_derived]
        impl #rt::Knobs for #name {
            type Overrides = #overrides;

            fn resolve_knobs(d: &Self, o: &#overrides) -> Self {
                Self { #(#resolve)* }
            }

            fn merge_knobs(into: &mut #overrides, patch: &#overrides) {
                #(#merge)*
            }

            fn knobs_from_form(
                prefix: &str,
                form: &#rt::FormInput<'_>,
                errs: &mut ::std::vec::Vec<crate::settings::FieldError>,
            ) -> #overrides {
                #overrides { #(#from_form)* }
            }

            fn check_knob_ranges(
                &self,
                prefix: &str,
                errs: &mut ::std::vec::Vec<crate::settings::FieldError>,
            ) {
                let _ = (prefix, &errs);
                #(#ranges)*
            }

            fn drop_out_of_range(
                o: &mut #overrides,
                prefix: &str,
                errs: &mut ::std::vec::Vec<crate::settings::FieldError>,
            ) {
                let _ = (&o, prefix, &errs);
                #(#drops)*
            }

            fn visit_knobs(prefix: &str, out: &mut ::std::vec::Vec<#rt::KnobMeta>) {
                let _ = (prefix, &out);
                #(#visit)*
            }
        }

        #presence_impl
    })
}

fn struct_attrs(input: &DeriveInput) -> syn::Result<StructAttrs> {
    let mut overrides = format_ident!("{}Overrides", input.ident);
    let mut presence = false;
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("knobs")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("overrides") {
                overrides = meta.value()?.parse()?;
            } else if meta.path.is_ident("presence") {
                presence = true;
            } else {
                return Err(meta.error("unknown #[knobs] attribute"));
            }
            Ok(())
        })?;
    }
    Ok(StructAttrs {
        overrides,
        presence,
    })
}

fn field_spec(field: &syn::Field) -> syn::Result<Field> {
    let ident = field
        .ident
        .clone()
        .ok_or_else(|| syn::Error::new_spanned(field, "named field expected"))?;
    let mut spec = Field {
        ident,
        ty: field.ty.clone(),
        kind: Kind::Plain,
        form: true,
        restart: false,
        min: None,
        max: None,
        aliases: Vec::new(),
    };
    for attr in &field.attrs {
        if attr.path().is_ident("knob") {
            attr.parse_nested_meta(|meta| {
                let set_kind = |spec: &mut Field, kind: Kind| {
                    if matches!(spec.kind, Kind::Plain) {
                        spec.kind = kind;
                        Ok(())
                    } else {
                        Err(meta.error("a knob has at most one kind"))
                    }
                };
                if meta.path.is_ident("nested") {
                    set_kind(&mut spec, Kind::Nested)
                } else if meta.path.is_ident("presence") {
                    let inner = option_inner(&field.ty).ok_or_else(|| {
                        syn::Error::new_spanned(&field.ty, "#[knob(presence)] needs Option<T>")
                    })?;
                    set_kind(&mut spec, Kind::Presence(Box::new(inner)))
                } else if meta.path.is_ident("required_profile") {
                    set_kind(&mut spec, Kind::RequiredProfile)
                } else if meta.path.is_ident("schema_version") {
                    set_kind(&mut spec, Kind::SchemaVersion)
                } else if meta.path.is_ident("form") {
                    let v: LitBool = meta.value()?.parse()?;
                    spec.form = v.value;
                    Ok(())
                } else if meta.path.is_ident("restart") {
                    spec.restart = true;
                    Ok(())
                } else if meta.path.is_ident("min") {
                    spec.min = Some(meta.value()?.parse()?);
                    Ok(())
                } else if meta.path.is_ident("max") {
                    spec.max = Some(meta.value()?.parse()?);
                    Ok(())
                } else {
                    Err(meta.error("unknown #[knob] attribute"))
                }
            })?;
        } else if attr.path().is_ident("serde") {
            attr.parse_nested_meta(|meta| {
                if meta.input.peek(Token![=]) {
                    let value: Expr = meta.value()?.parse()?;
                    if meta.path.is_ident("alias")
                        && let Expr::Lit(ExprLit {
                            lit: Lit::Str(s), ..
                        }) = value
                    {
                        spec.aliases.push(s);
                    }
                } else if meta.input.peek(syn::token::Paren) {
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let _: TokenStream2 = content.parse()?;
                }
                Ok(())
            })?;
        }
    }
    let structural = !matches!(spec.kind, Kind::Plain | Kind::RequiredProfile);
    if structural && (spec.min.is_some() || spec.max.is_some()) {
        return Err(syn::Error::new_spanned(
            &spec.ident,
            "min/max only apply to plain knobs",
        ));
    }
    Ok(spec)
}

fn is_bool(ty: &Type) -> bool {
    matches!(ty, Type::Path(p) if p.qself.is_none() && p.path.is_ident("bool"))
}

fn option_inner(ty: &Type) -> Option<Type> {
    let Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    if seg.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    match args.args.first()? {
        GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    }
}
