#![warn(clippy::pedantic)]

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::parse::Parser;
use syn::parse_macro_input;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::Expr;
use syn::FnArg;
use syn::ItemFn;
use syn::Pat;
use syn::ReturnType;
use syn::Token;
use syn::Type;

/// Marks a `fn main()` as the entry point for a cargo-rigtest test binary.
///
/// This is the recommended alternative to writing `fn main() { rigtest::run_main(); }`
/// by hand. The attributed function must be named `main`, take no arguments, and
/// have an empty body.
///
/// # Usage
///
/// Basic usage:
///
/// ```ignore
/// #[rigtest::main]
/// fn main() {}
/// ```
///
/// With HTTP client configuration (requires the `http-client` feature):
///
/// ```ignore
/// #[rigtest::main(http_client = configure_client)]
/// fn main() {}
///
/// fn configure_client(
///     builder: reqwest::ClientBuilder,
/// ) -> Result<reqwest::ClientBuilder, rigtest::Error> {
///     Ok(builder.danger_accept_invalid_certs(true))
/// }
/// ```
///
/// # HTTP client configure function
///
/// The function named by `http_client` must have the signature:
///
/// ```text
/// fn(reqwest::ClientBuilder) -> Result<reqwest::ClientBuilder, rigtest::Error>
/// ```
///
/// It receives a fresh `ClientBuilder`, applies any customisation, and returns
/// it wrapped in `Ok`. Returning `Err` causes every test subprocess to fail
/// immediately with the error message before any test logic runs. Configurations
/// that cannot fail should still wrap the builder in `Ok(...)` — the `Result`
/// return type is required so that fallible operations (such as loading a
/// certificate from disk) can be supported without a breaking API change.
///
/// # Compile errors
///
/// - The function must be named `main`.
/// - The function must take no arguments.
/// - The function body must be empty.
/// - The `http_client` parameter requires `rigtest` to be compiled with the
///   `http-client` feature; omitting it causes a missing-type compile error.
/// - The `ssh_client` parameter requires `rigtest` to be compiled with the
///   `ssh-client` feature and is only supported on Unix targets. On non-Unix
///   platforms the generated configurator static is omitted.
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);

    if func.sig.ident != "main" {
        return syn::Error::new_spanned(
            &func.sig.ident,
            "#[rigtest::main] must be applied to a function named `main`",
        )
        .to_compile_error()
        .into();
    }

    if !func.sig.inputs.is_empty() {
        return syn::Error::new_spanned(
            &func.sig.inputs,
            "#[rigtest::main] `fn main()` must take no arguments",
        )
        .to_compile_error()
        .into();
    }

    if !func.block.stmts.is_empty() {
        return syn::Error::new_spanned(
            &func.block,
            "#[rigtest::main] `fn main()` body must be empty — place configuration in a separate function referenced by the `http_client` parameter",
        )
        .to_compile_error()
        .into();
    }

    let metas = match syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
        .parse(attr)
    {
        Ok(m) => m,
        Err(e) => return e.to_compile_error().into(),
    };

    let mut http_client_fn: Option<syn::Expr> = None;
    let mut ssh_client_fn: Option<syn::Expr> = None;

    for meta in &metas {
        match meta {
            syn::Meta::NameValue(nv) if nv.path.is_ident("http_client") => {
                http_client_fn = Some(nv.value.clone());
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("ssh_client") => {
                ssh_client_fn = Some(nv.value.clone());
            }
            other => {
                return syn::Error::new_spanned(
                    other,
                    "unknown parameter for #[rigtest::main]; expected `http_client = <fn>` or `ssh_client = <fn>`",
                )
                .to_compile_error()
                .into();
            }
        }
    }

    let http_static = http_client_fn.map(|configure_fn| {
        quote! {
            #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_HTTP_CLIENT_CONFIGURATOR)]
            #[linkme(crate = ::rigtest::__linkme)]
            static __RIGTEST_HTTP_CLIENT_CONFIGURATOR: ::rigtest::registry::HttpClientConfiguratorEntry =
                ::rigtest::registry::HttpClientConfiguratorEntry::new(#configure_fn);
        }
    });

    let ssh_static = ssh_client_fn.map(|configure_fn| {
        quote! {
            #[cfg(unix)]
            #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_SSH_CLIENT_CONFIGURATOR)]
            #[linkme(crate = ::rigtest::__linkme)]
            static __RIGTEST_SSH_CLIENT_CONFIGURATOR: ::rigtest::registry::SshClientConfiguratorEntry =
                ::rigtest::registry::SshClientConfiguratorEntry::new(#configure_fn);
        }
    });

    let expanded = quote! {
        fn main() {
            ::rigtest::run_main();
        }

        #http_static
        #ssh_static
    };
    TokenStream::from(expanded)
}

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
/// | `tags = ["a", "b"]` | Attaches one or more string tags for use with the `--tag` and `--not-tag` CLI filters. |
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
///
/// # Parametrized cases
///
/// A test can be expanded into a table of cases by stacking one or more
/// `#[case(...)]` attributes above the function and tagging the parameters
/// that vary per row with `#[case]`. Each row becomes its own registered
/// `TestCase` with a unique name of the form `<fn>::case_<N>` (or
/// `<fn>::case_<N>_<label>` when the `#[case::label(...)]` form is used).
/// All `#[testcase]` flags (`serial`, `timeout`, `retries`) apply to every
/// generated row.
///
/// ```ignore
/// use std::sync::Arc;
/// use rigtest::{testcase, TestContext};
///
/// #[testcase]
/// #[case("alice", "admin")]
/// #[case::viewer("bob", "viewer")]
/// async fn user_has_expected_role(
///     _ctx: Arc<TestContext>,
///     #[case] user: &str,
///     #[case] expected_role: &str,
/// ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
///     assert!(!user.is_empty());
///     assert!(matches!(expected_role, "admin" | "viewer"));
///     Ok(())
/// }
/// ```
///
/// In the example above two tests are registered:
/// `user_has_expected_role::case_1` and
/// `user_has_expected_role::case_2_viewer`. Non-`#[case]` parameters (for
/// example `ctx`) are wired in as before; only `#[case]`-tagged parameters
/// receive per-row values.
#[proc_macro_attribute]
pub fn testcase(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_testcase(attr, item) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_testcase(attr: TokenStream, item: TokenStream) -> Result<TokenStream, syn::Error> {
    let mut func: ItemFn = syn::parse(item)?;
    let func_ident = func.sig.ident.clone();
    let func_name_str = func_ident.to_string();

    let TestcaseFlags {
        serial,
        timeout_tokens,
        retries_tokens,
        tags_tokens,
    } = parse_testcase_flags(attr)?;

    // Extract and strip stacked `#[case(...)]` / `#[case::label(...)]`
    // attributes from the function. Anything else stays on the re-emitted
    // function definition.
    let mut case_rows: Vec<CaseRow> = Vec::new();
    let mut other_attrs = Vec::with_capacity(func.attrs.len());
    for attr in func.attrs.drain(..) {
        match parse_case_attr(&attr) {
            Some(Ok(row)) => case_rows.push(row),
            Some(Err(err)) => return Err(err),
            None => other_attrs.push(attr),
        }
    }
    func.attrs = other_attrs;

    // Identify which positional parameters are tagged `#[case]` and strip
    // the marker so the re-emitted function compiles unchanged.
    let mut case_param_positions: Vec<usize> = Vec::new();
    for (idx, input) in func.sig.inputs.iter_mut().enumerate() {
        if let FnArg::Typed(pat_type) = input {
            let before = pat_type.attrs.len();
            pat_type.attrs.retain(|a| !a.path().is_ident("case"));
            if pat_type.attrs.len() != before {
                case_param_positions.push(idx);
            }
        }
    }

    validate_case_shape(&func, &case_rows, &case_param_positions)?;

    // No `#[case]` markers and no `#[case(...)]` rows → preserve the
    // historical single-test behaviour exactly.
    if case_rows.is_empty() {
        let static_ident = registration_ident(&func_name_str, None);
        let expanded = quote! {
            #[allow(clippy::unused_async)]
            #func

            #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_TEST_CASES)]
            #[linkme(crate = ::rigtest::__linkme)]
            static #static_ident: ::rigtest::registry::TestCase =
                ::rigtest::registry::TestCase::new(
                    #func_name_str,
                    module_path!(),
                    file!(),
                    #serial,
                    #timeout_tokens,
                    #retries_tokens,
                    #tags_tokens,
                    |ctx| ::std::boxed::Box::pin(async move { #func_ident(ctx).await }),
                );
        };
        return Ok(TokenStream::from(expanded));
    }

    let registrations = build_case_registrations(&CaseRegistrationInputs {
        func: &func,
        func_ident: &func_ident,
        func_name_str: &func_name_str,
        case_rows: &case_rows,
        case_param_positions: &case_param_positions,
        serial,
        timeout_tokens: &timeout_tokens,
        retries_tokens: &retries_tokens,
        tags_tokens: &tags_tokens,
    })?;

    let expanded = quote! {
        #[allow(clippy::unused_async)]
        #func

        #(#registrations)*
    };

    Ok(TokenStream::from(expanded))
}

struct TestcaseFlags {
    serial: bool,
    timeout_tokens: proc_macro2::TokenStream,
    retries_tokens: proc_macro2::TokenStream,
    tags_tokens: proc_macro2::TokenStream,
}

fn parse_testcase_flags(attr: TokenStream) -> Result<TestcaseFlags, syn::Error> {
    let metas = Punctuated::<syn::Meta, Token![,]>::parse_terminated
        .parse(attr)
        .unwrap_or_default();
    let mut serial = false;
    let mut timeout_tokens = quote! { None };
    let mut retries_tokens = quote! { 0u32 };
    let mut tags_tokens = quote! { &[] as &'static [&'static str] };
    for meta in &metas {
        match meta {
            syn::Meta::Path(p) if p.is_ident("serial") => serial = true,
            syn::Meta::NameValue(nv) if nv.path.is_ident("timeout") => {
                let val = &nv.value;
                timeout_tokens = quote! { Some(#val) };
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("retries") => {
                let val = &nv.value;
                retries_tokens = quote! { #val };
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("tags") => {
                tags_tokens = parse_tags(&nv.value)?;
            }
            _ => {}
        }
    }
    Ok(TestcaseFlags {
        serial,
        timeout_tokens,
        retries_tokens,
        tags_tokens,
    })
}

/// Parse the value of `tags = [...]` into a token stream that produces a
/// `&'static [&'static str]`.
///
/// Accepts an array literal of string literals. Each tag must be a non-empty
/// string with no whitespace — both are runner-side concerns surfaced as a
/// compile error so a typo in a tag does not silently match nothing at
/// runtime.
fn parse_tags(value: &syn::Expr) -> syn::Result<proc_macro2::TokenStream> {
    let syn::Expr::Array(array) = value else {
        return Err(syn::Error::new_spanned(
            value,
            "`tags` must be an array literal of string literals, e.g. tags = [\"smoke\", \"regression\"]",
        ));
    };

    let mut literals: Vec<syn::LitStr> = Vec::with_capacity(array.elems.len());
    for elem in &array.elems {
        let lit = match elem {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) => s.clone(),
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "`tags` entries must be string literals, e.g. \"smoke\"",
                ));
            }
        };

        let value = lit.value();
        if value.is_empty() {
            return Err(syn::Error::new_spanned(
                &lit,
                "`tags` entries must not be empty",
            ));
        }
        if value.chars().any(char::is_whitespace) {
            return Err(syn::Error::new_spanned(
                &lit,
                "`tags` entries must not contain whitespace",
            ));
        }
        literals.push(lit);
    }

    Ok(quote! { &[ #( #literals ),* ] as &'static [&'static str] })
}

/// Validate the relationship between `#[case(...)]` rows and `#[case]`
/// parameter markers, surfacing mismatches as actionable compile errors
/// pointing at the offending span.
fn validate_case_shape(
    func: &ItemFn,
    case_rows: &[CaseRow],
    case_param_positions: &[usize],
) -> Result<(), syn::Error> {
    if !case_rows.is_empty() && case_param_positions.is_empty() {
        return Err(syn::Error::new(
            case_rows[0].span,
            "#[case(...)] rows are present but no function parameter is tagged with #[case]; \
             add `#[case]` to each parameter that should receive a per-row value",
        ));
    }
    if case_rows.is_empty() && !case_param_positions.is_empty() {
        let span = func
            .sig
            .inputs
            .iter()
            .nth(case_param_positions[0])
            .map_or_else(Span::call_site, Spanned::span);
        return Err(syn::Error::new(
            span,
            "function parameter is tagged with #[case] but no #[case(...)] rows are stacked \
             above the function; add one or more `#[case(value, ...)]` attributes",
        ));
    }
    for row in case_rows {
        if row.values.len() != case_param_positions.len() {
            return Err(syn::Error::new(
                row.span,
                format!(
                    "#[case(...)] has {got} value(s) but the function has {want} #[case]-tagged \
                     parameter(s); every row must supply exactly one value per tagged parameter",
                    got = row.values.len(),
                    want = case_param_positions.len(),
                ),
            ));
        }
    }
    Ok(())
}

fn registration_ident(func_name: &str, suffix: Option<&str>) -> syn::Ident {
    let upper = func_name.to_uppercase().replace('-', "_");
    let name = if let Some(s) = suffix {
        format!(
            "__RIGTEST_TESTCASE_{upper}_{}",
            s.to_uppercase().replace('-', "_")
        )
    } else {
        format!("__RIGTEST_TESTCASE_{upper}")
    };
    syn::Ident::new(&name, Span::call_site())
}

struct CaseRegistrationInputs<'a> {
    func: &'a ItemFn,
    func_ident: &'a syn::Ident,
    func_name_str: &'a str,
    case_rows: &'a [CaseRow],
    case_param_positions: &'a [usize],
    serial: bool,
    timeout_tokens: &'a proc_macro2::TokenStream,
    retries_tokens: &'a proc_macro2::TokenStream,
    tags_tokens: &'a proc_macro2::TokenStream,
}

fn build_case_registrations(
    inputs: &CaseRegistrationInputs<'_>,
) -> Result<Vec<proc_macro2::TokenStream>, syn::Error> {
    let &CaseRegistrationInputs {
        func,
        func_ident,
        func_name_str,
        case_rows,
        case_param_positions,
        serial,
        timeout_tokens,
        retries_tokens,
        tags_tokens,
    } = inputs;
    // Reject more than one non-case parameter so the error fires at macro
    // expansion rather than as a confusing type mismatch later.
    let non_case_positions: Vec<usize> = (0..func.sig.inputs.len())
        .filter(|i| !case_param_positions.contains(i))
        .collect();
    if non_case_positions.len() > 1 {
        let span = func
            .sig
            .inputs
            .iter()
            .nth(non_case_positions[1])
            .map_or_else(Span::call_site, Spanned::span);
        return Err(syn::Error::new(
            span,
            "parametrized #[testcase] supports at most one non-#[case] parameter \
             (the `ctx: Arc<TestContext>` argument)",
        ));
    }

    let mut registrations = Vec::with_capacity(case_rows.len());
    for (i, row) in case_rows.iter().enumerate() {
        let index = i + 1;
        let suffix = match &row.label {
            Some(label) => format!("case_{index}_{label}"),
            None => format!("case_{index}"),
        };
        let case_name = format!("{func_name_str}::{suffix}");
        let static_ident = registration_ident(func_name_str, Some(&suffix));

        // Build the positional call: case values slot into the tagged
        // positions, `ctx` fills the (at most one) remaining position.
        let mut call_args: Vec<proc_macro2::TokenStream> =
            Vec::with_capacity(func.sig.inputs.len());
        let mut value_iter = row.values.iter();
        for idx in 0..func.sig.inputs.len() {
            if case_param_positions.contains(&idx) {
                let Some(val) = value_iter.next() else {
                    // Length already validated by `validate_case_shape`; reaching
                    // this branch would be an internal invariant break.
                    return Err(syn::Error::new(
                        row.span,
                        "internal error: case row value count mismatch",
                    ));
                };
                call_args.push(quote! { #val });
            } else {
                call_args.push(quote! { ctx });
            }
        }

        registrations.push(quote! {
            #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_TEST_CASES)]
            #[linkme(crate = ::rigtest::__linkme)]
            static #static_ident: ::rigtest::registry::TestCase =
                ::rigtest::registry::TestCase::new(
                    #case_name,
                    module_path!(),
                    file!(),
                    #serial,
                    #timeout_tokens,
                    #retries_tokens,
                    #tags_tokens,
                    |ctx| ::std::boxed::Box::pin(async move {
                        #func_ident(#(#call_args),*).await
                    }),
                );
        });
    }

    Ok(registrations)
}

/// A parsed `#[case(...)]` / `#[case::label(...)]` row.
struct CaseRow {
    /// Optional label following `case::`, used to disambiguate the
    /// generated test-name suffix.
    label: Option<String>,
    /// Positional values supplied for the row, one per `#[case]`-tagged
    /// parameter on the function signature.
    values: Vec<Expr>,
    /// Span of the original attribute, used for diagnostics.
    span: Span,
}

/// Recognise `#[case(...)]` / `#[case::label(...)]` attributes and parse
/// their positional argument list. Returns `None` for unrelated attributes.
fn parse_case_attr(attr: &syn::Attribute) -> Option<Result<CaseRow, syn::Error>> {
    let path = attr.path();
    let segments: Vec<&syn::PathSegment> = path.segments.iter().collect();
    let (label, is_case) = match segments.as_slice() {
        [seg] if seg.ident == "case" => (None, true),
        [first, second] if first.ident == "case" => (Some(second.ident.to_string()), true),
        _ => (None, false),
    };
    if !is_case {
        return None;
    }

    let span = attr.span();
    let values_result = attr
        .parse_args_with(Punctuated::<Expr, Token![,]>::parse_terminated)
        .map(|p| p.into_iter().collect::<Vec<_>>());

    Some(match values_result {
        Ok(values) => Ok(CaseRow {
            label,
            values,
            span,
        }),
        Err(e) => Err(e),
    })
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
            ::rigtest::registry::GlobalSetupEntry::new(
                || {
                    ::std::boxed::Box::pin(async {
                        ::std::boxed::Box::new(#func_ident().await)
                            as ::std::boxed::Box<dyn ::std::any::Any + Send + Sync>
                    })
                },
                |boxed| {
                    let concrete = boxed
                        .downcast_ref::<#return_type>()
                        .expect("cargo-rigtest: global_setup serialize type mismatch");
                    ::rigtest::__serde_json::to_string(concrete)
                        .expect("cargo-rigtest: failed to serialize global state")
                },
                |s| {
                    let concrete = ::rigtest::__serde_json::from_str::<#return_type>(s)
                        .expect("cargo-rigtest: failed to deserialize global state");
                    ::std::boxed::Box::new(concrete)
                        as ::std::boxed::Box<dyn ::std::any::Any + Send + Sync>
                },
            );
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
            ::rigtest::registry::GlobalTeardownEntry::new(|boxed| {
                ::std::boxed::Box::pin(async move {
                    let concrete = *boxed
                        .downcast::<#param_type>()
                        .expect("global_teardown type mismatch");
                    #func_ident(concrete).await
                })
            });
    };

    TokenStream::from(expanded)
}

/// Registers a function as the suite-wide preflight check.
///
/// The annotated function runs once in the coordinator before
/// `#[global_setup]` and before any test subprocess is spawned. It declares
/// the external dependencies the suite needs — TCP endpoints, environment
/// variables, DNS records, HTTP endpoints, SSH hosts, and custom checks —
/// by building a `rigtest::Preflight` value and returning it.
///
/// At most one `#[preflight]` may be defined per test binary. If any
/// declared probe fails, the coordinator prints a readiness table, exits
/// with status `2`, and skips both `#[global_setup]` and `#[global_teardown]`.
///
/// # Signature
///
/// `#[preflight]` accepts the signature:
///
/// ```text
/// fn name() -> Preflight { ... }
/// ```
///
/// `async fn`, additional parameters, and return types other than
/// `Preflight` are rejected at compile time with an actionable message.
///
/// # Example
///
/// ```ignore
/// use rigtest::Preflight;
/// use std::time::Duration;
///
/// #[rigtest::preflight]
/// fn preflight() -> Preflight {
///     Preflight::new()
///         .tcp("api", "127.0.0.1:8080")
///         .timeout(Duration::from_millis(500))
///         .env("home_set", "HOME")
/// }
/// ```
#[proc_macro_attribute]
pub fn preflight(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_preflight(attr, item) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_preflight(attr: TokenStream, item: TokenStream) -> Result<TokenStream, syn::Error> {
    let attr2: proc_macro2::TokenStream = attr.into();
    if !attr2.is_empty() {
        return Err(syn::Error::new_spanned(
            attr2,
            "#[preflight] does not accept any arguments",
        ));
    }

    let func: ItemFn = syn::parse(item)?;

    if func.sig.asyncness.is_some() {
        return Err(syn::Error::new_spanned(
            func.sig.asyncness,
            "#[preflight] functions must be synchronous (the framework runs probes; the \
             builder function only constructs the Preflight description)",
        ));
    }

    if !func.sig.inputs.is_empty() {
        return Err(syn::Error::new_spanned(
            &func.sig.inputs,
            "#[preflight] functions must take no arguments",
        ));
    }

    // Insist on an explicit `-> Preflight` return type. We deliberately
    // match by trailing path segment so both `Preflight` and the fully
    // qualified `rigtest::Preflight` are accepted; this is consistent
    // with `#[global_setup]`/`#[global_teardown]`, which surface the
    // return type's tokens verbatim.
    let return_ty = match &func.sig.output {
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &func.sig,
                "#[preflight] functions must return `Preflight`",
            ));
        }
        ReturnType::Type(_, ty) => ty.as_ref(),
    };
    if !return_type_is_preflight(return_ty) {
        return Err(syn::Error::new_spanned(
            return_ty,
            "#[preflight] functions must return `Preflight`",
        ));
    }

    let func_ident = &func.sig.ident;
    let static_ident = syn::Ident::new(
        &format!(
            "__RIGTEST_PREFLIGHT_{}",
            func_ident.to_string().to_uppercase()
        ),
        Span::call_site(),
    );

    let expanded = quote! {
        #func

        #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_PREFLIGHT)]
        #[linkme(crate = ::rigtest::__linkme)]
        static #static_ident: ::rigtest::registry::PreflightEntry =
            ::rigtest::registry::PreflightEntry::new(#func_ident);
    };
    Ok(TokenStream::from(expanded))
}

/// Returns true when `ty` is `Preflight` or a path ending in `::Preflight`.
fn return_type_is_preflight(ty: &Type) -> bool {
    let Type::Path(tp) = ty else {
        return false;
    };
    if tp.qself.is_some() {
        return false;
    }
    tp.path
        .segments
        .last()
        .is_some_and(|seg| seg.ident == "Preflight")
}
