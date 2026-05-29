#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::parse::Parser;
use syn::parse_macro_input;
use syn::FnArg;
use syn::ItemFn;
use syn::Pat;

/// Registers an async test function with the cargo-rigtest runtime.
///
/// Accepts optional flags:
/// - `serial` — prevent concurrent execution with any other test
/// - `timeout = <const Duration>` — kill and fail the test if it exceeds the duration
/// - `retries = <N>` — retry a failed test up to N additional times
///
/// Flags can be combined:
/// ```ignore
/// #[testcase(serial, timeout = Duration::from_secs(30), retries = 2)]
/// async fn my_test(ctx: Arc<TestContext>) -> Result<(), rigtest::Error> { ... }
/// ```
///
/// # Timeout and teardown
///
/// When a `timeout` fires the test subprocess is hard-killed. Any teardown
/// registered with [`TestContext::teardown`] will **not** run. Resources that
/// must be released regardless of outcome (open connections, temp files, etc.)
/// should be managed at a higher level — for example in `#[global_teardown]`,
/// via OS-level cleanup (Drop impls, RAII guards), or by an external fixture
/// that outlives the test process.
#[proc_macro_attribute]
pub fn testcase(attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    let func_ident = &func.sig.ident;
    let func_name_str = func_ident.to_string();

    // Parse comma-separated meta items: serial, timeout = <expr>, retries = <expr>
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

    // Build a unique static name: __RIGTEST_TESTCASE_SOME_FUNCTION_NAME
    let static_ident = syn::Ident::new(
        &format!(
            "__RIGTEST_TESTCASE_{}",
            func_name_str.to_uppercase().replace('-', "_")
        ),
        Span::call_site(),
    );

    let expanded = quote! {
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

/// Registers an async global setup function with the cargo-rigtest runtime.
///
/// The annotated function must have the signature:
/// ```ignore
/// async fn name() -> SomeType
/// ```
/// The return value is stored in the `TestContext` and made available to tests.
/// At most one `#[global_setup]` function may be defined in the test binary.
#[proc_macro_attribute]
pub fn global_setup(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    let func_ident = &func.sig.ident;

    let return_type = match &func.sig.output {
        syn::ReturnType::Default => quote! { () },
        syn::ReturnType::Type(_, ty) => quote! { #ty },
    };

    let expanded = quote! {
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

/// Registers an async global teardown function with the cargo-rigtest runtime.
///
/// The annotated function must have the signature:
/// ```ignore
/// async fn name(state: SomeType)
/// ```
/// The `SomeType` must match the return type of the corresponding
/// `#[global_setup]` function. At most one `#[global_teardown]` function
/// may be defined in the test binary.
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
