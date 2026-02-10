
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, FnArg, ImplItem, ItemImpl, ReturnType, Type};

#[proc_macro_attribute]
pub fn actor(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut input = parse_macro_input!(item as ItemImpl);
    let actor_type = &input.self_ty;
    let actor_name = if let Type::Path(tp) = actor_type.as_ref() {
        tp.path.segments.last().unwrap().ident.clone()
    } else {
        return syn::Error::new_spanned(
            actor_type,
            "Actor must be a simple type path"
        ).to_compile_error().into();
    };

    let msg_enum_name = format_ident!("{}Msg", actor_name);
    let ref_struct_name = format_ident!("{}Ref", actor_name);

    let mut msg_variants = Vec::new();
    let mut variant_names = Vec::new();
    let mut handle_matches = Vec::new();
    let mut ref_methods = Vec::new();
    let mut seen_variants = std::collections::HashSet::new();

    let mut on_start_hook = None;

    let mut on_restart_hook = None;
    let mut on_stop_hook = None;

    for item in &mut input.items {
        if let ImplItem::Fn(method) = item {
            let method_name_str = method.sig.ident.to_string();
            match method_name_str.as_str() {

                "on_start" => {
                    on_start_hook = Some(quote! {
                        async fn on_start(&mut self, ctx: &mut ::axactor::Context) {
                            <#actor_type>::on_start(self, ctx).await;
                        }
                    });
                },
                "on_restart" => {
                    on_restart_hook = Some(quote! {
                        async fn on_restart(&mut self, ctx: &mut ::axactor::Context) {
                            <#actor_type>::on_restart(self, ctx).await;
                        }
                    });
                },
                "on_stop" => {
                    on_stop_hook = Some(quote! {
                        async fn on_stop(&mut self, ctx: &mut ::axactor::Context) {
                            <#actor_type>::on_stop(self, ctx).await;
                        }
                    });
                },

                _ => {}
            }

            let is_msg = method.attrs.iter().any(|attr| attr.path().is_ident("msg"));

            if !is_msg {
                continue;
            }

            let method_name_str = method.sig.ident.to_string();
            let reserved = [
                "new", "stop", "stop_wait",
                "mailbox_len", "idle_for", "silence_for",
                "inflight_count", "incarnation", "is_closed", "is_terminated",
            ];

            if reserved.contains(&method_name_str.as_str()) {
                return syn::Error::new_spanned(
                    &method.sig.ident,
                    format!("'{}' is reserved by axactor ref API; choose a different #[msg] name", method_name_str),
                ).to_compile_error().into();
            }

            let banned_suffixes = ["_wait", "_timeout", "_wait_timeout"];
            if banned_suffixes.iter().any(|s| method_name_str.ends_with(s)) {
                return syn::Error::new_spanned(
                    &method.sig.ident,
                    "message method name must not end with _wait/_timeout/_wait_timeout (reserved by generated APIs)",
                ).to_compile_error().into();
            }

            method.attrs.retain(|attr| !attr.path().is_ident("msg"));

            let method_name = &method.sig.ident;
            let variant_name_str = snake_to_camel(&method_name.to_string());
            if !seen_variants.insert(variant_name_str.clone()) {
                return syn::Error::new_spanned(
                    method_name,
                    format!("Variant name collision detected: '{}' maps to already existing variant '{}'", method_name_str, variant_name_str)
                ).to_compile_error().into();
            }
            let variant_name = format_ident!("{}", variant_name_str);
            variant_names.push(variant_name.clone());

            let mut sig_args = Vec::new();
            let mut variant_fields = Vec::new();
            let mut call_args = Vec::new();

            let mut args_iter = method.sig.inputs.iter();

            match args_iter.next() {
                Some(FnArg::Receiver(_)) => {}
                _ => {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "Actor method must take &mut self as the first argument"
                    ).to_compile_error().into();
                }
            }

            let mut has_context = false;
            let mut is_first_arg = true;

            for arg in args_iter {
                match arg {
                    FnArg::Typed(pat_type) => {
                        let pat = &pat_type.pat;
                        let ty = &pat_type.ty;

                        let ident = match pat.as_ref() {
                            syn::Pat::Ident(p) if p.by_ref.is_none() && p.subpat.is_none() => &p.ident,
                            _ => {
                                return syn::Error::new_spanned(
                                    &pat,
                                    "actor #[msg] args must be simple identifiers (e.g. x: T)"
                                ).to_compile_error().into();
                            }
                        };

                        if is_context_ty(ty.as_ref()) {
                            if is_first_arg {
                                has_context = true;
                                is_first_arg = false;
                                continue;
                            } else {
                                return syn::Error::new_spanned(
                                    &pat_type.ty,
                                    "Context must be the first argument after &mut self"
                                ).to_compile_error().into();
                            }
                        }

                        sig_args.push(quote! { #ident: #ty });
                        variant_fields.push(quote! { #ident: #ty });
                        call_args.push(quote! { #ident });
                        is_first_arg = false;
                    }

                    _ => {}
                }
            }

            let return_type = match &method.sig.output {
                ReturnType::Default => quote! { () },
                ReturnType::Type(_, ty) => quote! { #ty },
            };

            let is_void = if let ReturnType::Default = method.sig.output { true } else { false };

            if is_void {
                let is_async = method.sig.asyncness.is_some();

                msg_variants.push(quote! {
                    #variant_name { #( #variant_fields ),* }
                });

                let call_expr = if has_context {
                    if call_args.is_empty() {
                        if is_async { quote! { self.#method_name(ctx).await } } else { quote! { self.#method_name(ctx) } }
                    } else {
                        if is_async { quote! { self.#method_name(ctx, #( #call_args ),*).await } } else { quote! { self.#method_name(ctx, #( #call_args ),*) } }
                    }
                } else {
                    if is_async { quote! { self.#method_name(#( #call_args ),*).await } } else { quote! { self.#method_name(#( #call_args ),*) } }
                };

                handle_matches.push(quote! {
                    #msg_enum_name::#variant_name { #( #call_args ),* } => {
                        #call_expr;
                    }
                });

                let wait_method_name = format_ident!("{}_wait", method_name);
                ref_methods.push(quote! {
                    pub fn #method_name(&self, #( #sig_args ),*) -> Result<(), ::axactor::TellError> {
                        self.tx.try_send(#msg_enum_name::#variant_name { #( #call_args ),* })
                    }

                    pub async fn #wait_method_name(&self, #( #sig_args ),*) -> Result<(), ::axactor::TellError> {
                        self.tx.send_wait(#msg_enum_name::#variant_name { #( #call_args ),* }).await
                    }
                });
            } else {
                let is_async = method.sig.asyncness.is_some();

                msg_variants.push(quote! {
                    #variant_name { #( #variant_fields, )* __reply: ::tokio::sync::oneshot::Sender<#return_type> }
                });

                let call_expr = if has_context {
                    if call_args.is_empty() {
                        if is_async { quote! { self.#method_name(ctx).await } } else { quote! { self.#method_name(ctx) } }
                    } else {
                        if is_async { quote! { self.#method_name(ctx, #( #call_args ),*).await } } else { quote! { self.#method_name(ctx, #( #call_args ),*) } }
                    }
                } else {
                    if is_async { quote! { self.#method_name(#( #call_args ),*).await } } else { quote! { self.#method_name(#( #call_args ),*) } }
                };

                handle_matches.push(quote! {
                    #msg_enum_name::#variant_name { #( #call_args, )* __reply } => {
                        let res = #call_expr;
                        let _ = __reply.send(res);
                    }
                });

                let wait_method_name = format_ident!("{}_wait", method_name);
                let wait_timeout_method_name = format_ident!("{}_wait_timeout", method_name);
                let timeout_method_name = format_ident!("{}_timeout", method_name);

                ref_methods.push(quote! {
                    pub async fn #method_name(&self, #( #sig_args ),*) -> Result<#return_type, ::axactor::AskError> {
                        let (tx, rx) = ::tokio::sync::oneshot::channel();
                        self.tx.try_send(#msg_enum_name::#variant_name { #( #call_args, )* __reply: tx })
                            .map_err(|e| match e {
                                ::axactor::TellError::Full => ::axactor::AskError::Full,
                                ::axactor::TellError::Closed => ::axactor::AskError::Closed,
                            })?;
                        rx.await.map_err(|_| ::axactor::AskError::Canceled)
                    }

                    pub async fn #wait_method_name(&self, #( #sig_args ),*) -> Result<#return_type, ::axactor::AskError> {
                        self.tx.ask_wait(|__reply| #msg_enum_name::#variant_name { #( #call_args, )* __reply }).await
                    }

                    pub async fn #wait_timeout_method_name(&self, #( #sig_args, )* __timeout: ::std::time::Duration) -> Result<#return_type, ::axactor::AskError> {
                        self.tx.ask_wait_timeout(|__reply| #msg_enum_name::#variant_name { #( #call_args, )* __reply }, __timeout).await
                    }

                    pub async fn #timeout_method_name(&self, #( #sig_args, )* __timeout: ::std::time::Duration) -> Result<#return_type, ::axactor::AskError> {
                        ::tokio::time::timeout(__timeout, self.#method_name(#( #call_args ),*)).await
                            .map_err(|_| ::axactor::AskError::Timeout)?
                    }

                });

            }
        }
    }

    let gen = quote! {
        #input

        pub enum #msg_enum_name {

            __Stop,
            #( #msg_variants ),*
        }

        impl ::axactor::MessageKind for #msg_enum_name {
            fn kind(&self) -> &'static str {
                match self {
                    #msg_enum_name::__Stop => concat!(stringify!(#msg_enum_name), "::__Stop"),
                    #( #msg_enum_name::#variant_names { .. } => concat!(stringify!(#msg_enum_name), "::", stringify!(#variant_names)), )*
                }
            }
        }

        #[derive(Clone)]
        pub struct #ref_struct_name {
            tx: ::axactor::MailboxTx<#msg_enum_name>,
        }

        impl #ref_struct_name {
            pub fn new(tx: ::axactor::MailboxTx<#msg_enum_name>) -> Self {
                Self { tx }
            }

            pub fn stop(&self) -> Result<(), ::axactor::TellError> {
                self.tx.try_send(#msg_enum_name::__Stop)
            }

            pub async fn stop_wait(&self) -> Result<(), ::axactor::TellError> {
                self.tx.send_wait(#msg_enum_name::__Stop).await
            }

            pub fn mailbox_len(&self) -> usize {
                self.tx.len()
            }

            pub fn idle_for(&self) -> ::std::time::Duration {
                self.tx.idle_for()
            }

            pub fn silence_for(&self) -> ::std::time::Duration {
                self.tx.silence_for()
            }

            pub fn inflight_count(&self) -> usize {
                self.tx.inflight_count()
            }

            pub fn incarnation(&self) -> u64 {
                self.tx.incarnation()
            }

            pub fn is_closed(&self) -> bool {
                self.tx.is_closed()
            }

            pub fn is_terminated(&self) -> bool {
                self.tx.is_terminated()
            }

            #( #ref_methods )*

        }

        impl ::axactor::RefMetrics for #ref_struct_name {
            fn mailbox_len(&self) -> usize {
                self.mailbox_len()
            }

            fn inflight_count(&self) -> usize {
                self.tx.inflight_count()
            }

            fn idle_for(&self) -> ::std::time::Duration {
                self.idle_for()
            }

            fn silence_for(&self) -> ::std::time::Duration {
                self.silence_for()
            }
            fn incarnation(&self) -> u64 {
                self.incarnation()
            }
            fn is_closed(&self) -> bool {
                self.is_closed()
            }
            fn is_terminated(&self) -> bool {
                self.is_terminated()
            }

            fn stop(&self) -> Result<(), ::axactor::TellError> {
                self.stop()
            }
        }

        impl From<::axactor::MailboxTx<#msg_enum_name>> for #ref_struct_name {
            fn from(tx: ::axactor::MailboxTx<#msg_enum_name>) -> Self {
                Self::new(tx)
            }
        }

        #[::async_trait::async_trait]
        impl ::axactor::rt::Actor for #actor_type {
            type Msg = #msg_enum_name;

            #on_start_hook
            #on_restart_hook
            #on_stop_hook

            async fn handle(&mut self, ctx: &mut ::axactor::Context, msg: Self::Msg) {
                match msg {
                    #msg_enum_name::__Stop => {
                        ctx.stop();
                    }
                    #( #handle_matches ),*
                }
            }
        }

    };

    gen.into()
}

fn snake_to_camel(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;
    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else {
            if capitalize_next {
                result.push(c.to_uppercase().next().unwrap());
                capitalize_next = false;
            } else {
                result.push(c);
            }
        }
    }
    result
}

fn is_context_ty(ty: &syn::Type) -> bool {
    if let syn::Type::Reference(tr) = ty {
        if let syn::Type::Path(tp) = tr.elem.as_ref() {
            return tp.path.segments.last().map(|s| s.ident == "Context").unwrap_or(false);
        }
    }
    false
}