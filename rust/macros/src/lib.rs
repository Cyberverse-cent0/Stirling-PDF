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
//! Generated handler behaviour, identical to the hand-written pattern it
//! replaces:
//! 1. `receive_multipart` parses the body.
//! 2. `?async=true` (query or multipart field) → `jobs::submit` returns the
//!    job JSON immediately and the work runs on a spawned task.
//! 3. Otherwise the work runs inline — always inside `spawn_blocking`,
//!    because lopdf parsing/serialising is CPU-bound and would stall the
//!    async runtime.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn};

#[proc_macro_attribute]
pub fn endpoint(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);
    expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(mut input: ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    if input.sig.asyncness.is_some() {
        return Err(syn::Error::new_spanned(
            &input.sig.fn_token,
            "#[endpoint] work functions must be synchronous; \
             the macro already runs them on the blocking pool",
        ));
    }
    if input.sig.inputs.len() != 4 {
        return Err(syn::Error::new_spanned(
            &input.sig.inputs,
            "#[endpoint] work functions take exactly 4 arguments: \
             (state, query, uploads, params)",
        ));
    }

    let name = input.sig.ident.clone();
    let work_name = syn::Ident::new(&format!("{name}_work"), name.span());
    input.sig.ident = work_name.clone();

    Ok(quote! {
        #input

        pub async fn #name(
            axum::extract::State(state): axum::extract::State<crate::handlers::jobs::AppState>,
            axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
            multipart: axum::extract::Multipart,
        ) -> crate::error::AppResult<axum::response::Response> {
            let (uploads, params) = crate::upload::receive_multipart(multipart).await?;
            if crate::handlers::jobs::async_requested(&query, &params) {
                let work_state = state.clone();
                return crate::handlers::jobs::submit(
                    &state,
                    async move {
                        tokio::task::spawn_blocking(move || {
                            #work_name(work_state, query, uploads, params)
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
                #work_name(state, query, uploads, params)
                    .map(axum::response::IntoResponse::into_response)
            })
            .await
            .map_err(|e| crate::error::AppError::Internal(anyhow::Error::new(e)))?
        }
    })
}
