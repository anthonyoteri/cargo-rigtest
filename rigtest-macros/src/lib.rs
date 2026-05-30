#![warn(clippy::pedantic)]

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::parse::Parser;
use syn::parse_macro_input;
use syn::FnArg;
use syn::ItemFn;
use syn::Pat;

/// Registers an async function as a cargo-rigtest test case.
///
/// The annotated function must have the signature:
///
/// ```text
/// async fn name(ctx: Arc<TestContext>) -> Result<(), rigtest::Error> { ... }
/// ```
///
/// The `ctx` parameter gives access to global setup data and per-test
/// lifecycle hooks. The function name becomes the test name that appears in
/// output and `--filter` expressions.
///
/// # Flags
///
/// All flags are optional and can be combined in any order.
///
/// | Flag | Description |
/// |------|-------------|
/// | `serial` | Prevents concurrent execution with any other test. |
/// | `timeout = <Duration>` | Kills and fails the test if it exceeds the given duration. |
/// | `retries = <N>` | Retries a failed test up to `N` additional times before reporting failure. |
///
/// # Examples
///
/// Minimal test with no flags:
///
/// ```ignore
/// use std::sync::Arc;
/// use rigtest::{testcase, TestContext};
///
/// #[testcase]
/// async fn addition_works(_ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
///     assert_eq!(1 + 1, 2);
///     Ok(())
/// }
/// ```
///
/// Test with a timeout, retries, and the `serial` flag:
///
/// ```ignore
/// use std::sync::Arc;
/// use std::time::Duration;
/// use rigtest::{testcase, TestContext};
///
/// #[testcase(serial, timeout = Duration::from_secs(30), retries = 2)]
/// async fn exclusive_network_probe(_ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
///     // network call
///     Ok(())
/// }
/// ```
///
/// # Timeout and teardown
///
/// When a `timeout` fires the test subprocess is terminated. Any teardown
/// registered with `TestContext::teardown` will **not** run. Resources that
/// must be released regardless of outcome should be managed in
/// `#[global_teardown]`, which runs in the coordinator process outside the
/// killed subprocess.
#[proc_macro_attribute]
pub fn testcase(attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    let func_ident = &func.sig.ident;
    let func_name_str = func_ident.to_string();

    let metas = syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
        .parse(attr)
        .unwrap_or_default();

    let mut serial = false;
    let mut timeout_tokens = quote! { None };
    let mut retries_tokens = quote! { 0u32 };

    for meta in &metas {
        match meta {
            syn::Meta::Path(p) if p.is_ident("serial") => {
                serial = true;
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("timeout") => {
                let val = &nv.value;
                timeout_tokens = quote! { Some(#val) };
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("retries") => {
                let val = &nv.value;
                retries_tokens = quote! { #val };
            }
            _ => {}
        }
    }

    let static_ident = syn::Ident::new(
        &format!(
            "__RIGTEST_TESTCASE_{}",
            func_name_str.to_uppercase().replace('-', "_")
        ),
        Span::call_site(),
    );

    let expanded = quote! {
        #[allow(clippy::unused_async)]
        #func

        #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_TEST_CASES)]
        #[linkme(crate = ::rigtest::__linkme)]
        static #static_ident: ::rigtest::registry::TestCase =
            ::rigtest::registry::TestCase {
                name: #func_name_str,
                module: module_path!(),
                file: file!(),
                serial: #serial,
                timeout: #timeout_tokens,
                retries: #retries_tokens,
                test_fn: |ctx| ::std::boxed::Box::pin(async move { #func_ident(ctx).await }),
            };
    };

    TokenStream::from(expanded)
}

/// Registers an async function as the global setup hook for a test binary.
///
/// The annotated function runs once before any tests and its return value is
/// made available to every test through `TestContext::global_data`. At most
/// one `#[global_setup]` function may be defined in a single test binary.
///
/// The annotated function must have the signature:
///
/// ```text
/// async fn name() -> SomeType { ... }
/// ```
///
/// `SomeType` must implement both `serde::Serialize` and
/// `serde::de::DeserializeOwned` so the runtime can pass the state to each
/// test subprocess via an environment variable.
///
/// # Examples
///
/// ```ignore
/// use rigtest::global_setup;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize)]
/// pub struct Config {
///     pub db_url: String,
///     pub api_key: String,
/// }
///
/// #[global_setup]
/// async fn setup() -> Config {
///     Config {
///         db_url: std::env::var("DB_URL")
///             .unwrap_or_else(|_| "postgres://localhost/test".into()),
///         api_key: std::env::var("API_KEY").expect("API_KEY must be set"),
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn global_setup(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    let func_ident = &func.sig.ident;

    let return_type = match &func.sig.output {
        syn::ReturnType::Default => quote! { () },
        syn::ReturnType::Type(_, ty) => quote! { #ty },
    };

    let expanded = quote! {
        #[allow(clippy::unused_async)]
        #func

        #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_GLOBAL_SETUP)]
        #[linkme(crate = ::rigtest::__linkme)]
        static __RIGTEST_GLOBAL_SETUP: ::rigtest::registry::GlobalSetupEntry =
            ::rigtest::registry::GlobalSetupEntry {
                setup_fn: || {
                    ::std::boxed::Box::pin(async {
                        ::std::boxed::Box::new(#func_ident().await)
                            as ::std::boxed::Box<dyn ::std::any::Any + Send + Sync>
                    })
                },
                serialize_fn: |boxed| {
                    let concrete = boxed
                        .downcast_ref::<#return_type>()
                        .expect("cargo-rigtest: global_setup serialize type mismatch");
                    ::rigtest::__serde_json::to_string(concrete)
                        .expect("cargo-rigtest: failed to serialize global state")
                },
                deserialize_fn: |s| {
                    let concrete = ::rigtest::__serde_json::from_str::<#return_type>(s)
                        .expect("cargo-rigtest: failed to deserialize global state");
                    ::std::boxed::Box::new(concrete)
                        as ::std::boxed::Box<dyn ::std::any::Any + Send + Sync>
                },
            };
    };

    TokenStream::from(expanded)
}

/// Registers an async function as the global teardown hook for a test binary.
///
/// The annotated function runs once after all tests have finished. It receives
/// the value produced by `#[global_setup]` and is responsible for releasing
/// any resources allocated during setup. At most one `#[global_teardown]`
/// function may be defined in a single test binary.
///
/// The annotated function must have the signature:
///
/// ```text
/// async fn name(state: SomeType) { ... }
/// ```
///
/// `SomeType` must match the return type of the corresponding
/// `#[global_setup]` function.
///
/// # Examples
///
/// ```ignore
/// use rigtest::global_teardown;
///
/// // `Config` is the type returned by the matching `#[global_setup]` function.
/// #[global_teardown]
/// async fn teardown(cfg: Config) {
///     println!("releasing resources for {}", cfg.db_url);
///     // close connections, delete temp data, etc.
/// }
/// ```
///
/// # Panics
///
/// Panics at compile time if the annotated function does not have exactly one
/// typed parameter.
#[proc_macro_attribute]
pub fn global_teardown(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    let func_ident = &func.sig.ident;

    // Extract the type of the first parameter (skipping `self`).
    let param_type = func
        .sig
        .inputs
        .iter()
        .find_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                // Make sure this is not a self-like pattern.
                match pat_type.pat.as_ref() {
                    Pat::Ident(_) => Some(pat_type.ty.as_ref().clone()),
                    _ => None,
                }
            } else {
                None
            }
        })
        .expect("#[global_teardown] function must have exactly one typed parameter");

    let expanded = quote! {
        #[allow(clippy::unused_async)]
        #func

        #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_GLOBAL_TEARDOWN)]
        #[linkme(crate = ::rigtest::__linkme)]
        static __RIGTEST_GLOBAL_TEARDOWN: ::rigtest::registry::GlobalTeardownEntry =
            ::rigtest::registry::GlobalTeardownEntry {
                teardown_fn: |boxed| {
                    ::std::boxed::Box::pin(async move {
                        let concrete = *boxed
                            .downcast::<#param_type>()
                            .expect("global_teardown type mismatch");
                        #func_ident(concrete).await
                    })
                },
            };
    };

    TokenStream::from(expanded)
}
