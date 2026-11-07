//! `#[endpoint]` — turns a synchronous work function into an axum multipart
//! handler with the standard `?async=true` job dispatch.
//!
//! ```ignore
//! #[stirling_macros::endpoint]
//! fn rotate_pdf(
//!     state: AppState,
//!     query: HashMap<String, String>,
//!     uploads: Vec<Upload>,
//!     params: Vec<(String, String)>,
//! ) -> AppResult<Response> {
//!     // PDF work — runs on tokio's blocking pool, never on async workers.
//! }
//! ```
//!
//! The macro emits two items: the work fn renamed `<name>_work` (same
//! signature, must be synchronous and return `AppResult<T>` where
//! `T: IntoResponse`), and `pub async fn <name>` with the axum signature
//! `(State<AppState>, Query<HashMap<String, String>>, Multipart)`. The
//! `query` argument passed to the work fn is the destructured
//! `HashMap<String, String>`, not the `Query` wrapper.
//!
//! Attribute arguments select the multipart receiver and work signature:
//!
//! - `#[endpoint]` — `receive_multipart`; work takes
//!   `(state, query, uploads, params)`.
//! - `#[endpoint(files = "attachments,stampImage")]` —
//!   `receive_files_multipart`; work takes
//!   `(state, query, uploads, extras, params)` where `extras` is
//!   `Vec<(String, Upload)>` of the named file parts.
//! - `#[endpoint(receive = "some::path")]` — a custom async fn
//!   `fn(Multipart) -> AppResult<(Vec<Upload>, Vec<(String, Upload)>,
//!   Vec<(String, String)>)>`; same five-argument work signature as
//!   `files`. For endpoints whose parts do not fit `fileInput` + named
//!   extras (e.g. `/api/v1/form/*` binds `file` plus `data`/`names`/
//!   `fields`).
//!
//! Generated handler behaviour, identical to the hand-written pattern it
//! replaces:
//! 1. The receiver parses the body.
//! 2. `?async=true` (query or multipart field) → `jobs::submit` returns the
//!    job JSON immediately and the work runs on a spawned task.
//! 3. Otherwise the work runs inline — always inside `spawn_blocking`,
//!    because lopdf parsing/serialising is CPU-bound and would stall the
//!    async runtime.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{parse_macro_input, Expr, ItemFn, LitStr, MetaNameValue, Token};

#[proc_macro_attribute]
pub fn endpoint(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);
    let args = match EndpointArgs::parse(attr.into()) {
        Ok(args) => args,
        Err(e) => return e.into_compile_error().into(),
    };
    expand(input, args)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

struct EndpointArgs {
    files: Option<Vec<LitStr>>,
    receive: Option<Expr>,
}

impl EndpointArgs {
    fn parse(attr: proc_macro2::TokenStream) -> syn::Result<Self> {
        let mut files = None;
        let mut receive = None;
        let pairs = Punctuated::<MetaNameValue, Token![,]>::parse_terminated.parse2(attr)?;
        for pair in pairs {
            let name = pair.path.get_ident().map(|i| i.to_string());
            let value = match &pair.value {
                syn::Expr::Lit(lit) => match &lit.lit {
                    syn::Lit::Str(s) => s.clone(),
                    _ => {
                        return Err(syn::Error::new_spanned(
                            &pair.value,
                            "expected a string literal",
                        ))
                    }
                },
                _ => {
                    return Err(syn::Error::new_spanned(
                        &pair.value,
                        "expected a string literal",
                    ))
                }
            };
            match name.as_deref() {
                Some("files") => {
                    if files.is_some() {
                        return Err(syn::Error::new_spanned(
                            pair.path,
                            "duplicate `files` argument",
                        ));
                    }
                    files = Some(
                        value
                            .value()
                            .split(',')
                            .map(|s| LitStr::new(s.trim(), value.span()))
                            .collect(),
                    );
                }
                Some("receive") => {
                    if receive.is_some() {
                        return Err(syn::Error::new_spanned(
                            pair.path,
                            "duplicate `receive` argument",
                        ));
                    }
                    receive = Some(syn::parse_str::<Expr>(&value.value())?);
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        pair.path,
                        "unknown #[endpoint] argument; expected `files` or `receive`",
                    ))
                }
            }
        }
        if files.is_some() && receive.is_some() {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "`files` and `receive` are mutually exclusive",
            ));
        }
        Ok(Self { files, receive })
    }

    /// The receiver call producing `(uploads, extras, params)` — or
    /// `(uploads, params)` in the default mode — and whether the work fn
    /// takes the extras argument.
    fn receiver(&self) -> (proc_macro2::TokenStream, bool) {
        if let Some(files) = &self.files {
            (
                quote! {
                    crate::upload::receive_files_multipart(multipart, &[#(#files),*]).await?
                },
                true,
            )
        } else if let Some(receive) = &self.receive {
            (quote! { #receive(multipart).await? }, true)
        } else {
            (
                quote! { crate::upload::receive_multipart(multipart).await? },
                false,
            )
        }
    }
}

fn expand(mut input: ItemFn, args: EndpointArgs) -> syn::Result<proc_macro2::TokenStream> {
    if input.sig.asyncness.is_some() {
        return Err(syn::Error::new_spanned(
            input.sig.fn_token,
            "#[endpoint] work functions must be synchronous; \
             the macro already runs them on the blocking pool",
        ));
    }
    let (receive, with_extras) = args.receiver();
    let expected = if with_extras { 5 } else { 4 };
    if input.sig.inputs.len() != expected {
        let shape = if with_extras {
            "(state, query, uploads, extras, params)"
        } else {
            "(state, query, uploads, params)"
        };
        return Err(syn::Error::new_spanned(
            &input.sig.inputs,
            format!("#[endpoint] work functions take exactly {expected} arguments: {shape}"),
        ));
    }

    let name = input.sig.ident.clone();
    let work_name = syn::Ident::new(&format!("{name}_work"), name.span());
    input.sig.ident = work_name.clone();

    let (destructure, extras_arg) = if with_extras {
        (
            quote! { let (uploads, extras, params) = #receive; },
            quote! { extras, },
        )
    } else {
        (quote! { let (uploads, params) = #receive; }, quote! {})
    };

    Ok(quote! {
        #input

        pub async fn #name(
            axum::extract::State(state): axum::extract::State<crate::handlers::jobs::AppState>,
            axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
            multipart: axum::extract::Multipart,
        ) -> crate::error::AppResult<axum::response::Response> {
            #destructure
            if crate::handlers::jobs::async_requested(&query, &params) {
                let work_state = state.clone();
                return crate::handlers::jobs::submit(
                    &state,
                    async move {
                        tokio::task::spawn_blocking(move || {
                            #work_name(work_state, query, uploads, #extras_arg params)
                                .map(axum::response::IntoResponse::into_response)
                        })
                        .await
                        .map_err(|e| {
                            crate::error::AppError::Internal(anyhow::Error::new(e))
                        })?
                    },
                )
                .await;
            }
            tokio::task::spawn_blocking(move || {
                #work_name(state, query, uploads, #extras_arg params)
                    .map(axum::response::IntoResponse::into_response)
            })
            .await
            .map_err(|e| crate::error::AppError::Internal(anyhow::Error::new(e)))?
        }
    })
}
