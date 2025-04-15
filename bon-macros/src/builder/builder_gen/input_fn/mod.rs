mod validation;

use super::models::{AssocMethodReceiverCtxParams, FinishFnParams};
use super::top_level_config::TopLevelConfig;
use super::{
    AssocMethodCtxParams, BuilderGenCtx, FinishFnBody, Generics, Member, MemberOrigin, RawMember,
};
use crate::builder::builder_gen::models::{BuilderGenCtxParams, BuilderTypeParams, StartFnParams};
use crate::normalization::{GenericsNamespace, NormalizeSelfTy, SyntaxVariant};
use crate::parsing::{ItemSigConfig, SpannedKey};
use crate::util::prelude::*;
use std::borrow::Cow;
use std::rc::Rc;
use syn::punctuated::Punctuated;
use syn::visit_mut::VisitMut;

pub(crate) struct FnInputCtx<'a> {
    namespace: &'a GenericsNamespace,
    fn_item: SyntaxVariant<syn::ItemFn>,
    impl_ctx: Option<Rc<ImplCtx>>,
    config: TopLevelConfig,

    start_fn: StartFnParams,
    self_ty_prefix: Option<String>,
}

pub(crate) struct FnInputCtxParams<'a> {
    pub(crate) namespace: &'a GenericsNamespace,
    pub(crate) fn_item: SyntaxVariant<syn::ItemFn>,
    pub(crate) impl_ctx: Option<Rc<ImplCtx>>,
    pub(crate) config: TopLevelConfig,
}

pub(crate) struct ImplCtx {
    pub(crate) self_ty: Box<syn::Type>,
    pub(crate) generics: syn::Generics,

    /// Lint suppressions from the original item that will be inherited by all items
    /// generated by the macro. If the original syntax used `#[expect(...)]`,
    /// then it must be represented as `#[allow(...)]` here.
    pub(crate) allow_attrs: Vec<syn::Attribute>,
}

impl<'a> FnInputCtx<'a> {
    pub(crate) fn new(params: FnInputCtxParams<'a>) -> Result<Self> {
        let start_fn = params.config.start_fn.clone();

        let start_fn_ident = start_fn
            .name
            .map(SpannedKey::into_value)
            .unwrap_or_else(|| {
                let fn_ident = &params.fn_item.norm.sig.ident;

                // Special case for the method named `new`. We rename it to `builder`
                // since this is the name that is conventionally used by starting
                // function in the builder pattern. We also want to make
                // the `#[builder]` attribute on the method `new` fully compatible
                // with deriving a builder from a struct.
                if params.impl_ctx.is_some() && fn_ident == "new" {
                    syn::Ident::new("builder", fn_ident.span())
                } else {
                    fn_ident.clone()
                }
            });

        let start_fn = StartFnParams {
            ident: start_fn_ident,

            vis: start_fn.vis.map(SpannedKey::into_value),

            docs: start_fn
                .docs
                .map(SpannedKey::into_value)
                .unwrap_or_else(|| {
                    params
                        .fn_item
                        .norm
                        .attrs
                        .iter()
                        .filter(|attr| attr.is_doc_expr())
                        .cloned()
                        .collect()
                }),

            // Override on the start fn to use the generics from the
            // target function itself. We must not duplicate the generics
            // from the impl block here
            generics: Some(Generics::new(
                params
                    .fn_item
                    .norm
                    .sig
                    .generics
                    .params
                    .iter()
                    .cloned()
                    .collect(),
                params.fn_item.norm.sig.generics.where_clause.clone(),
            )),
        };

        let self_ty_prefix = params.impl_ctx.as_deref().and_then(|impl_ctx| {
            let prefix = impl_ctx
                .self_ty
                .as_path()?
                .path
                .segments
                .last()?
                .ident
                .to_string();

            Some(prefix)
        });

        let ctx = Self {
            namespace: params.namespace,
            fn_item: params.fn_item,
            impl_ctx: params.impl_ctx,
            config: params.config,
            self_ty_prefix,
            start_fn,
        };

        ctx.validate()?;

        Ok(ctx)
    }

    fn assoc_method_ctx(&self) -> Result<Option<AssocMethodCtxParams>> {
        let self_ty = match self.impl_ctx.as_deref() {
            Some(impl_ctx) => impl_ctx.self_ty.clone(),
            None => return Ok(None),
        };

        Ok(Some(AssocMethodCtxParams {
            self_ty,
            receiver: self.assoc_method_receiver_ctx_params()?,
        }))
    }

    fn assoc_method_receiver_ctx_params(&self) -> Result<Option<AssocMethodReceiverCtxParams>> {
        let receiver = match self.fn_item.norm.sig.receiver() {
            Some(receiver) => receiver,
            None => return Ok(None),
        };

        let builder_attr_on_receiver = receiver
            .attrs
            .iter()
            .find(|attr| attr.path().is_ident("builder"));

        if let Some(attr) = builder_attr_on_receiver {
            bail!(
                attr,
                "#[builder] attributes on the receiver are not supported"
            );
        }

        let self_ty = match self.impl_ctx.as_deref() {
            Some(impl_ctx) => &impl_ctx.self_ty,
            None => return Ok(None),
        };

        let mut without_self_keyword = receiver.ty.clone();

        NormalizeSelfTy { self_ty }.visit_type_mut(&mut without_self_keyword);

        Ok(Some(AssocMethodReceiverCtxParams {
            with_self_keyword: receiver.clone(),
            without_self_keyword,
        }))
    }

    fn generics(&self) -> Generics {
        let impl_ctx = self.impl_ctx.as_ref();
        let norm_fn_params = &self.fn_item.norm.sig.generics.params;
        let params = impl_ctx
            .map(|impl_ctx| merge_generic_params(&impl_ctx.generics.params, norm_fn_params))
            .unwrap_or_else(|| norm_fn_params.iter().cloned().collect());

        let where_clauses = [
            self.fn_item.norm.sig.generics.where_clause.clone(),
            impl_ctx.and_then(|impl_ctx| impl_ctx.generics.where_clause.clone()),
        ];

        let where_clause = where_clauses
            .into_iter()
            .flatten()
            .reduce(|mut combined, clause| {
                combined.predicates.extend(clause.predicates);
                combined
            });

        Generics::new(params, where_clause)
    }

    pub(crate) fn adapted_fn(&self) -> Result<syn::ItemFn> {
        let mut orig = self.fn_item.orig.clone();

        if let Some(name) = self.config.start_fn.name.as_deref() {
            if *name == orig.sig.ident {
                bail!(
                    &name,
                    "the starting function name must be different from the name \
                    of the positional function under the #[builder] attribute"
                )
            }
        } else {
            // By default the original positional function becomes hidden.
            orig.vis = syn::Visibility::Inherited;

            // Remove all doc comments from the function itself to avoid docs duplication
            // which may lead to duplicating doc tests, which in turn implies repeated doc
            // tests execution, which means worse tests performance.
            //
            // We don't do this for the case when the positional function is exposed
            // alongside the builder which implies that the docs should be visible
            // as the function itself is visible.
            orig.attrs.retain(|attr| !attr.is_doc_expr());

            let bon = &self.config.bon;

            orig.attrs.extend([
                syn::parse_quote!(#[doc(hidden)]),
                // We don't rename the function immediately, but instead defer the renaming
                // to a later stage. This is because the original name of the function can
                // be used by other macros that may need a stable identifier.
                //
                // For example, if `#[tracing::instrument]` is placed on the function,
                // the function name will be used as a span name.
                syn::parse_quote!(#[#bon::__::__privatize]),
            ]);
        }

        // Remove any `#[builder]` attributes that were meant for this proc macro.
        orig.attrs.retain(|attr| !attr.path().is_ident("builder"));

        // Remove all doc comments attributes from function arguments, because they are
        // not valid in that position in regular Rust code. The cool trick is that they
        // are still valid syntactically when a proc macro like this one pre-processes
        // them and removes them from the expanded code. We use the doc comments to put
        // them on the generated setter methods.
        //
        // We also strip all `builder(...)` attributes because this macro processes them
        // and they aren't needed in the output.
        for arg in &mut orig.sig.inputs {
            arg.attrs_mut()
                .retain(|attr| !attr.is_doc_expr() && !attr.path().is_ident("builder"));
        }

        orig.attrs.push(syn::parse_quote!(#[allow(
            // It's fine if there are too many positional arguments in the function
            // because the whole purpose of this macro is to fight with this problem
            // at the call site by generating a builder, while keeping the fn definition
            // site the same with tons of positional arguments which don't harm readability
            // there because their names are explicitly specified at the definition site.
            clippy::too_many_arguments,

            // It's fine to use many bool arguments in the function signature because
            // all of them will be named at the call site when the builder is used.
            clippy::fn_params_excessive_bools,
        )]));

        Ok(orig)
    }

    pub(crate) fn into_builder_gen_ctx(self) -> Result<BuilderGenCtx> {
        let assoc_method_ctx = self.assoc_method_ctx()?;

        let members = self
            .fn_item
            .apply_ref(|fn_item| fn_item.sig.inputs.iter().filter_map(syn::FnArg::as_typed))
            .into_iter()
            .map(|arg| {
                let pat = match arg.norm.pat.as_ref() {
                    syn::Pat::Ident(pat) => pat,
                    _ => bail!(
                        &arg.orig.pat,
                        "use a simple `identifier: type` syntax for the function argument; \
                        destructuring patterns in arguments aren't supported by the `#[builder]`",
                    ),
                };

                let ty = SyntaxVariant {
                    norm: arg.norm.ty.clone(),
                    orig: arg.orig.ty.clone(),
                };

                Ok(RawMember {
                    attrs: &arg.norm.attrs,
                    ident: pat.ident.clone(),
                    ty,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let members = Member::from_raw(&self.config, MemberOrigin::FnArg, members)?;

        let generics = self.generics();
        let mut adapted_fn_sig = self.adapted_fn()?.sig;

        if self.config.start_fn.name.is_none() {
            crate::privatize::privatize_fn_name(&mut adapted_fn_sig);
        }

        let finish_fn_body = FnCallBody {
            sig: adapted_fn_sig,
            impl_ctx: self.impl_ctx.clone(),
        };

        let ItemSigConfig {
            name: finish_fn_ident,
            vis: finish_fn_vis,
            docs: finish_fn_docs,
        } = self.config.finish_fn;

        let is_special_builder_method = self.impl_ctx.is_some()
            && (self.fn_item.norm.sig.ident == "new" || self.fn_item.norm.sig.ident == "builder");

        let finish_fn_ident = finish_fn_ident
            .map(SpannedKey::into_value)
            .unwrap_or_else(|| {
                // For `builder` methods the `build` finisher is more conventional
                if is_special_builder_method {
                    format_ident!("build")
                } else {
                    format_ident!("call")
                }
            });

        let finish_fn_docs = finish_fn_docs
            .map(SpannedKey::into_value)
            .unwrap_or_else(|| {
                vec![syn::parse_quote! {
                    /// Finishes building and performs the requested action.
                }]
            });

        let finish_fn = FinishFnParams {
            ident: finish_fn_ident,
            vis: finish_fn_vis.map(SpannedKey::into_value),
            unsafety: self.fn_item.norm.sig.unsafety,
            asyncness: self.fn_item.norm.sig.asyncness,
            special_attrs: get_propagated_attrs(&self.fn_item.norm.attrs)?,
            body: Box::new(finish_fn_body),
            output: self.fn_item.norm.sig.output,
            attrs: finish_fn_docs,
        };

        let fn_allows = self
            .fn_item
            .norm
            .attrs
            .iter()
            .filter_map(syn::Attribute::to_allow);

        let allow_attrs = self
            .impl_ctx
            .as_ref()
            .into_iter()
            .flat_map(|impl_ctx| impl_ctx.allow_attrs.iter().cloned())
            .chain(fn_allows)
            .collect();

        let builder_ident = || {
            let user_override = self.config.builder_type.name.map(SpannedKey::into_value);

            if let Some(user_override) = user_override {
                return user_override;
            }

            let ty_prefix = self.self_ty_prefix.unwrap_or_default();

            // A special case for the `new` or `builder` method.
            // We don't insert the `Builder` suffix in this case because
            // this special case should be compatible with deriving
            // a builder from a struct.
            //
            // We can arrive inside of this branch only if the function under
            // the macro is called `new` or `builder`.
            if is_special_builder_method {
                return format_ident!("{ty_prefix}Builder");
            }

            let pascal_case_fn = self.fn_item.norm.sig.ident.snake_to_pascal_case();

            format_ident!("{ty_prefix}{pascal_case_fn}Builder")
        };

        let builder_type = BuilderTypeParams {
            ident: builder_ident(),
            derives: self.config.derive,
            docs: self.config.builder_type.docs.map(SpannedKey::into_value),
            vis: self.config.builder_type.vis.map(SpannedKey::into_value),
        };

        BuilderGenCtx::new(BuilderGenCtxParams {
            bon: self.config.bon,
            namespace: Cow::Borrowed(self.namespace),
            members,

            allow_attrs,

            const_: self.config.const_,
            on: self.config.on,

            assoc_method_ctx,
            generics,
            orig_item_vis: self.fn_item.norm.vis,

            builder_type,
            state_mod: self.config.state_mod,
            start_fn: self.start_fn,
            finish_fn,
        })
    }
}

struct FnCallBody {
    sig: syn::Signature,
    impl_ctx: Option<Rc<ImplCtx>>,
}

impl FinishFnBody for FnCallBody {
    fn generate(&self, ctx: &BuilderGenCtx) -> TokenStream {
        let asyncness = &self.sig.asyncness;
        let maybe_await = asyncness.is_some().then(|| quote!(.await));

        // Filter out lifetime generic arguments, because they are not needed
        // to be specified explicitly when calling the function. This also avoids
        // the problem that it's not always possible to specify lifetimes in
        // the turbofish syntax. See the problem of late-bound lifetimes specification
        // in the issue https://github.com/rust-lang/rust/issues/42868
        let generic_args = self
            .sig
            .generics
            .params
            .iter()
            .filter(|arg| !matches!(arg, syn::GenericParam::Lifetime(_)))
            .map(syn::GenericParam::to_generic_argument);

        let prefix = ctx
            .assoc_method_ctx
            .as_ref()
            .and_then(|ctx| {
                let ident = &ctx.receiver.as_ref()?.field_ident;
                Some(quote!(self.#ident.))
            })
            .or_else(|| {
                let self_ty = &self.impl_ctx.as_deref()?.self_ty;
                Some(quote!(<#self_ty>::))
            });

        let fn_ident = &self.sig.ident;

        // The variables with values of members are in scope for this expression.
        let member_vars = ctx.members.iter().map(Member::orig_ident);

        quote! {
            #prefix #fn_ident::<#(#generic_args,)*>(
                #( #member_vars ),*
            )
            #maybe_await
        }
    }
}

/// To merge generic params we need to make sure lifetimes are always the first
/// in the resulting list according to Rust syntax restrictions.
fn merge_generic_params(
    left: &Punctuated<syn::GenericParam, syn::Token![,]>,
    right: &Punctuated<syn::GenericParam, syn::Token![,]>,
) -> Vec<syn::GenericParam> {
    let is_lifetime = |param: &&_| matches!(param, &&syn::GenericParam::Lifetime(_));

    let (left_lifetimes, left_rest): (Vec<_>, Vec<_>) = left.iter().partition(is_lifetime);
    let (right_lifetimes, right_rest): (Vec<_>, Vec<_>) = right.iter().partition(is_lifetime);

    left_lifetimes
        .into_iter()
        .chain(right_lifetimes)
        .chain(left_rest)
        .chain(right_rest)
        .cloned()
        .collect()
}

const PROPAGATED_ATTRIBUTES: &[&str] = &["must_use", "track_caller"];

fn get_propagated_attrs(attrs: &[syn::Attribute]) -> Result<Vec<syn::Attribute>> {
    PROPAGATED_ATTRIBUTES
        .iter()
        .copied()
        .filter_map(|needle| find_propagated_attr(attrs, needle).transpose())
        .collect()
}

fn find_propagated_attr(attrs: &[syn::Attribute], needle: &str) -> Result<Option<syn::Attribute>> {
    let mut iter = attrs
        .iter()
        .filter(|attr| attr.meta.path().is_ident(needle));

    let result = iter.next();

    if let Some(second) = iter.next() {
        bail!(
            second,
            "found multiple #[{}], but bon only works with exactly one or zero.",
            needle
        );
    }

    if let Some(attr) = result {
        if let syn::AttrStyle::Inner(_) = attr.style {
            bail!(
                attr,
                "#[{}] attribute must be placed on the function itself, \
                not inside it.",
                needle
            );
        }
    }

    Ok(result.cloned())
}
