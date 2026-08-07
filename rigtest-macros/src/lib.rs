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
/// With a suite-wide default timeout applied to every test that does not set
/// its own `timeout` (and is not marked `#[testcase(no_timeout)]`):
///
/// ```ignore
/// #[rigtest::main(default_timeout = std::time::Duration::from_secs(60))]
/// fn main() {}
/// ```
///
/// # Suite-wide default timeout
///
/// `default_timeout = <expr>` takes any expression evaluating to a
/// `std::time::Duration` and applies it to every test case as a fallback
/// timeout. Precedence: a per-case `#[testcase(timeout = …)]` overrides the
/// default; `#[testcase(no_timeout)]` forces no timeout even when a default
/// is set. At most one `default_timeout` may be declared per test binary.
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
    let mut default_timeout_expr: Option<syn::Expr> = None;

    for meta in &metas {
        match meta {
            syn::Meta::NameValue(nv) if nv.path.is_ident("http_client") => {
                http_client_fn = Some(nv.value.clone());
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("ssh_client") => {
                ssh_client_fn = Some(nv.value.clone());
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("default_timeout") => {
                default_timeout_expr = Some(nv.value.clone());
            }
            other => {
                return syn::Error::new_spanned(
                    other,
                    "unknown parameter for #[rigtest::main]; expected `http_client = <fn>`, `ssh_client = <fn>`, or `default_timeout = <Duration>`",
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

    let default_timeout_static = default_timeout_expr.map(|timeout| {
        quote! {
            #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_DEFAULT_TIMEOUT)]
            #[linkme(crate = ::rigtest::__linkme)]
            static __RIGTEST_DEFAULT_TIMEOUT: ::rigtest::registry::DefaultTimeoutEntry =
                ::rigtest::registry::DefaultTimeoutEntry::new(#timeout);
        }
    });

    let expanded = quote! {
        fn main() {
            ::rigtest::run_main();
        }

        #http_static
        #ssh_static
        #default_timeout_static
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
/// Any parameter that is neither the `Arc<TestContext>` argument nor a
/// `#[case]` parameter is resolved as a [`fixture`] by name: its identifier
/// must match a `#[fixture]` in scope, and it receives that fixture's
/// returned value. See [`fixture`] for setup/teardown semantics.
///
/// # Flags
///
/// All flags are optional and can be combined in any order.
///
/// | Flag | Description |
/// |------|-------------|
/// | `serial` | Fully exclusive: this test runs alone, never concurrently with any other test (including grouped tests). |
/// | `serial = "group"` | Names a serial group. Tests sharing a group name never run concurrently with each other; different groups (and ungrouped tests) may run in parallel. The group name must be a string literal. |
/// | `timeout = <Duration>` | Kills and fails the test if it exceeds the given duration. |
/// | `no_timeout` | Opts this test out of any suite-wide `default_timeout`, forcing no timeout. Cannot be combined with `timeout = …`. |
/// | `retries = <N>` | Retries a failed test up to `N` additional times before reporting failure. |
/// | `retry_on_error = <pat>` | Only retry when the test's typed `Err(_)` matches the pattern (same syntax as `matches!`). Requires the function to return `Result<(), ConcreteType>`. |
/// | `tags = ["a", "b"]` | Attaches one or more string tags for use with the `--tag` and `--not-tag` CLI filters. |
///
/// # Timeout precedence
///
/// A per-case `timeout = …` always wins. When a suite-wide
/// `#[rigtest::main(default_timeout = …)]` is declared it applies to every
/// test that does not set its own `timeout`; `no_timeout` opts a test out of
/// that default entirely (forcing no timeout). Combining `no_timeout` with
/// `timeout = …` is a compile error:
///
/// ```compile_fail
/// use std::sync::Arc;
/// use std::time::Duration;
/// use rigtest::{testcase, TestContext};
///
/// #[testcase(no_timeout, timeout = Duration::from_secs(1))]
/// async fn conflicting(_ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
///     Ok(())
/// }
/// # fn main() {}
/// ```
///
/// # The `retry_on_error` matcher
///
/// `retry_on_error = <pattern>` takes any Rust pattern accepted by the
/// standard library's `matches!` macro — including alternatives with `|`
/// and `if` guards — and pattern-matches the test's typed `Err(_)` value
/// before the error is boxed. When the pattern matches, the failure is
/// eligible for retry as usual; when it does not, the test fails
/// immediately regardless of how many retries remain. Panics, timeouts,
/// and subprocess kills are never retried when a matcher is in force.
///
/// The compiler rejects `retry_on_error` with `Result<(), rigtest::Error>`
/// / `Result<(), Box<dyn Error + Send + Sync>>` / `Result<(), BoxError>`:
/// pattern-matching on a boxed trait object is meaningless, and the
/// matcher needs the concrete error type to splice into `matches!`. The
/// rejection message points at the expected signature.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use rigtest::{testcase, TestContext};
///
/// // `retry_on_error` requires a concrete error type, not `rigtest::Error`.
/// #[testcase(retry_on_error = _)]
/// async fn no_box_dyn_error(_ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
///     Ok(())
/// }
/// # fn main() {}
/// ```
///
/// # Serial groups
///
/// A named serial group (`serial = "group"`) requires a string literal;
/// a bare identifier or other expression is rejected at compile time.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use rigtest::{testcase, TestContext};
///
/// // The group name must be a string literal, not a bare identifier.
/// #[testcase(serial = db)]
/// async fn bad_group(_ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
///     Ok(())
/// }
/// # fn main() {}
/// ```
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
///
/// ## Value lists with `#[values]`
///
/// A parameter can instead be tagged `#[values(v1, v2, ...)]` to enumerate
/// the values it should take. Every `#[values]` parameter is an independent
/// dimension, and the generated cases are the **cartesian product** across
/// all of them. Tagging the same parameter with both `#[case]` and
/// `#[values]`, or writing an empty `#[values()]`, is a compile error.
///
/// ```ignore
/// use std::sync::Arc;
/// use rigtest::{testcase, TestContext};
///
/// #[testcase]
/// async fn method_status(
///     _ctx: Arc<TestContext>,
///     #[values("GET", "POST", "PUT")] method: &str,
///     #[values(200, 404)] status: u16,
/// ) -> Result<(), rigtest::Error> {
///     assert!(!method.is_empty());
///     assert!(status >= 200);
///     Ok(())
/// }
/// ```
///
/// This registers `3 × 2 = 6` cases. `#[case(...)]` rows and `#[values]`
/// parameters compose: the case rows form the outermost dimension, then each
/// `#[values]` parameter left-to-right (the last varying fastest).
///
/// Each generated case is named `<fn>::case_<N>_<label>`, where `<N>` is the
/// 1-based index into the product and `<label>` is the sanitized,
/// underscore-joined rendering of the varying values (case-row label first,
/// then each chosen `#[values]` value with only `[A-Za-z0-9]` kept). When a
/// combination yields no label fragments the suffix is just `case_<N>`.
///
/// An empty `#[values()]` is rejected at compile time:
///
/// ```compile_fail
/// use std::sync::Arc;
/// use rigtest::{testcase, TestContext};
///
/// // `#[values(...)]` must list at least one value.
/// #[testcase]
/// async fn empty_values(_ctx: Arc<TestContext>, #[values()] n: u8)
///     -> Result<(), rigtest::Error> { Ok(()) }
/// # fn main() {}
/// ```
///
/// Tagging one parameter with both `#[case]` and `#[values]` is also
/// rejected — a parameter belongs to exactly one dimension:
///
/// ```compile_fail
/// use std::sync::Arc;
/// use rigtest::{testcase, TestContext};
///
/// #[testcase]
/// #[case(1)]
/// async fn both_markers(_ctx: Arc<TestContext>, #[case] #[values(2)] n: u8)
///     -> Result<(), rigtest::Error> { Ok(()) }
/// # fn main() {}
/// ```
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
        serial_group_tokens,
        no_timeout,
        timeout_tokens,
        retries_tokens,
        retry_on_error,
        tags_tokens,
    } = parse_testcase_flags(attr)?;

    if retry_on_error.is_some() {
        validate_retry_on_error_signature(&func)?;
    }

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

    // Identify which positional parameters are tagged `#[case]` /
    // `#[values(...)]` and strip the markers so the re-emitted function
    // compiles unchanged.
    let (case_param_positions, values_params) = collect_param_markers(&mut func)?;

    validate_case_shape(&func, &case_rows, &case_param_positions)?;

    let retry_on_error_set = retry_on_error.is_some();
    let retry_on_error_set_tokens = quote! { #retry_on_error_set };

    // No `#[case]`/`#[values]` dimensions → single test. The parameters are
    // classified into the ctx argument and any fixture arguments; a lone
    // `Arc<TestContext>` param keeps the historical single-test behavior
    // byte-for-byte.
    if case_rows.is_empty() && values_params.is_empty() {
        let static_ident = registration_ident(&func_name_str, None);
        let classes = classify_params(&func, &[], &[])?;
        let fixtures = fixture_idents(&classes);
        let call_args = build_call_args(&classes, &[], &[], &[], !fixtures.is_empty());
        let inner = build_testcase_body(&func_ident, &call_args, retry_on_error.as_ref());
        let body = wrap_fixtures(&inner, &fixtures);
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
                    #serial_group_tokens,
                    #timeout_tokens,
                    #no_timeout,
                    #retries_tokens,
                    #retry_on_error_set_tokens,
                    #tags_tokens,
                    |ctx| ::std::boxed::Box::pin(async move { #body }),
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
        values_params: &values_params,
        serial,
        serial_group_tokens: &serial_group_tokens,
        no_timeout,
        timeout_tokens: &timeout_tokens,
        retries_tokens: &retries_tokens,
        retry_on_error: retry_on_error.as_ref(),
        retry_on_error_set_tokens: &retry_on_error_set_tokens,
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
    serial_group_tokens: proc_macro2::TokenStream,
    no_timeout: bool,
    timeout_tokens: proc_macro2::TokenStream,
    retries_tokens: proc_macro2::TokenStream,
    /// When present, the user-supplied pattern from `retry_on_error = <pat>`.
    /// `None` when the matcher attribute is absent.
    retry_on_error: Option<syn::Pat>,
    tags_tokens: proc_macro2::TokenStream,
}

fn parse_testcase_flags(attr: TokenStream) -> Result<TestcaseFlags, syn::Error> {
    let metas = Punctuated::<syn::Meta, Token![,]>::parse_terminated
        .parse(attr)
        .unwrap_or_default();
    let mut serial = false;
    let mut serial_group_tokens = quote! { None };
    let mut no_timeout = false;
    let mut timeout_set = false;
    let mut timeout_tokens = quote! { None };
    let mut retries_tokens = quote! { 0u32 };
    let mut retry_on_error: Option<syn::Pat> = None;
    let mut tags_tokens = quote! { &[] as &'static [&'static str] };
    for meta in &metas {
        match meta {
            syn::Meta::Path(p) if p.is_ident("serial") => serial = true,
            syn::Meta::NameValue(nv) if nv.path.is_ident("serial") => {
                let group = parse_serial_group(&nv.value)?;
                serial_group_tokens = quote! { Some(#group) };
            }
            syn::Meta::Path(p) if p.is_ident("no_timeout") => no_timeout = true,
            syn::Meta::NameValue(nv) if nv.path.is_ident("timeout") => {
                let val = &nv.value;
                timeout_set = true;
                timeout_tokens = quote! { Some(#val) };
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("retries") => {
                let val = &nv.value;
                retries_tokens = quote! { #val };
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("retry_on_error") => {
                retry_on_error = Some(parse_retry_on_error_pattern(&nv.value)?);
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("tags") => {
                tags_tokens = parse_tags(&nv.value)?;
            }
            _ => {}
        }
    }
    if no_timeout && timeout_set {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[testcase]: `no_timeout` cannot be combined with `timeout = …`; \
             `no_timeout` forces no timeout (opting out of any suite-wide default), \
             while `timeout` sets an explicit one — pick one",
        ));
    }
    Ok(TestcaseFlags {
        serial,
        serial_group_tokens,
        no_timeout,
        timeout_tokens,
        retries_tokens,
        retry_on_error,
        tags_tokens,
    })
}

/// Parse the value of `serial = "group"` as a string literal naming the
/// serial group. A non-string value is rejected with a compile error so a
/// typo like `serial = db` fails loudly rather than silently.
fn parse_serial_group(value: &syn::Expr) -> syn::Result<syn::LitStr> {
    if let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(s),
        ..
    }) = value
    {
        Ok(s.clone())
    } else {
        Err(syn::Error::new_spanned(
            value,
            "`serial` group must be a string literal, e.g. serial = \"db\"",
        ))
    }
}

/// Parse the value of `retry_on_error = <pat>` as a Rust pattern, the same
/// syntax accepted by `match` arms and the `matches!` macro.
///
/// `syn::Meta::NameValue` stores values as expressions, so we re-emit the
/// caller's tokens and parse them as a [`syn::Pat`] with alternative
/// patterns enabled — that mirrors what the codegen later splices into
/// `matches!`.
fn parse_retry_on_error_pattern(value: &syn::Expr) -> syn::Result<syn::Pat> {
    let tokens = quote! { #value };
    syn::parse::Parser::parse2(syn::Pat::parse_multi_with_leading_vert, tokens).map_err(|e| {
        syn::Error::new(
            e.span(),
            format!(
                "`retry_on_error` must be a pattern, the same syntax accepted by `matches!`: {e}"
            ),
        )
    })
}

/// When `retry_on_error` is set, the user's test function must return
/// `Result<(), ConcreteType>` — a named error type the macro can name in
/// the generated `matches!` arm. Reject `Result<(), Box<dyn Error + …>>`,
/// `Result<(), rigtest::Error>`, and `Result<(), BoxError>` at compile
/// time with a message pointing at the signature.
fn validate_retry_on_error_signature(func: &ItemFn) -> Result<(), syn::Error> {
    let return_ty = match &func.sig.output {
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &func.sig,
                "#[testcase(retry_on_error = ...)] requires the test to return \
                 `Result<(), ConcreteType>` (a named error type the matcher can pattern-match); \
                 the function currently has no return type",
            ));
        }
        ReturnType::Type(_, ty) => ty.as_ref(),
    };

    let err_ty = result_err_type(return_ty).ok_or_else(|| {
        syn::Error::new_spanned(
            return_ty,
            "#[testcase(retry_on_error = ...)] requires the test to return \
             `Result<(), ConcreteType>` where `ConcreteType` is a named error type \
             (not `Box<dyn Error + Send + Sync>` / `rigtest::Error`); \
             switch the signature to a concrete error type so the matcher can \
             pattern-match on its variants",
        )
    })?;

    if err_type_is_unmatchable(err_ty) {
        return Err(syn::Error::new_spanned(
            err_ty,
            "#[testcase(retry_on_error = ...)] cannot match against a boxed trait object; \
             switch the return type to `Result<(), ConcreteType>` with a named error type \
             (for example a custom `#[derive(Debug)] enum MyError { Network, ... }`) so the \
             matcher can pattern-match on its variants",
        ));
    }

    Ok(())
}

/// If `ty` is `Result<(), E>` (or `core::result::Result<(), E>` /
/// `std::result::Result<(), E>` / `Result<E>` with `Ok = ()` defaulted),
/// returns the `E` type. Otherwise returns `None`. The macro only needs to
/// recognise the common spelling — operator-defined type aliases that
/// disguise the shape are out of scope, same as the rest of the macro's
/// signature validation.
fn result_err_type(ty: &Type) -> Option<&Type> {
    let Type::Path(tp) = ty else { return None };
    let seg = tp.path.segments.last()?;
    if seg.ident != "Result" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    let mut type_args = args.args.iter().filter_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    });
    let _ok = type_args.next()?;
    type_args.next()
}

/// Returns true when `ty` is a boxed `dyn Error + …` trait object or a
/// known type alias to one (`rigtest::Error` / `BoxError`). The macro
/// recognises these specific spellings so the most common signature
/// mistake — leaving the framework's default error type in place — is
/// caught at compile time. Aliases defined by the operator are out of
/// scope and surface later as a normal type mismatch in the
/// macro-generated `matches!` arm.
fn err_type_is_unmatchable(ty: &Type) -> bool {
    if type_is_box_dyn_error(ty) {
        return true;
    }
    let Type::Path(tp) = ty else { return false };
    let Some(last) = tp.path.segments.last() else {
        return false;
    };
    matches!(last.ident.to_string().as_str(), "Error" | "BoxError")
}

/// Returns true when `ty` is `Box<dyn Error + ...>` for any error-trait
/// path (e.g. `std::error::Error`, `core::error::Error`). The generic-arg
/// check is purely structural — anything inside the angle brackets that
/// names `Error` as the trait satisfies the check.
fn type_is_box_dyn_error(ty: &Type) -> bool {
    let Type::Path(tp) = ty else { return false };
    let Some(seg) = tp.path.segments.last() else {
        return false;
    };
    if seg.ident != "Box" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return false;
    };
    args.args.iter().any(|a| {
        let syn::GenericArgument::Type(Type::TraitObject(to)) = a else {
            return false;
        };
        to.bounds.iter().any(|b| {
            if let syn::TypeParamBound::Trait(tb) = b {
                tb.path.segments.last().is_some_and(|s| s.ident == "Error")
            } else {
                false
            }
        })
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
    values_params: &'a [ValuesParam],
    serial: bool,
    serial_group_tokens: &'a proc_macro2::TokenStream,
    no_timeout: bool,
    timeout_tokens: &'a proc_macro2::TokenStream,
    retries_tokens: &'a proc_macro2::TokenStream,
    retry_on_error: Option<&'a syn::Pat>,
    retry_on_error_set_tokens: &'a proc_macro2::TokenStream,
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
        values_params,
        serial,
        serial_group_tokens,
        no_timeout,
        timeout_tokens,
        retries_tokens,
        retry_on_error,
        retry_on_error_set_tokens,
        tags_tokens,
    } = inputs;
    // Classify each parameter into the ctx argument, a `#[case]` value, a
    // `#[values]` value, or a fixture (wired in by name — see
    // `wrap_fixtures`). `#[case]`/`#[values]` parameters receive their
    // per-combination values; `classify_params` also rejects a second
    // `Arc<TestContext>` and unsupported parameter shapes.
    let classes = classify_params(func, case_param_positions, values_params)?;
    let fixtures = fixture_idents(&classes);
    let has_fixtures = !fixtures.is_empty();

    // Case rows form the outermost dimension; a single implicit empty entry
    // stands in when there are no `#[case]` rows. Each `#[values]` param is
    // a further dimension, iterated left-to-right with the last varying
    // fastest.
    let case_entries: Vec<Option<&CaseRow>> = if case_rows.is_empty() {
        vec![None]
    } else {
        case_rows.iter().map(Some).collect()
    };
    let value_tuples = value_index_tuples(values_params);

    let mut registrations =
        Vec::with_capacity(case_entries.len().saturating_mul(value_tuples.len()));
    let mut index = 0usize;
    for case_entry in &case_entries {
        for tuple in &value_tuples {
            index += 1;

            // Assemble the label from the case-row label (if any) followed
            // by each varying value's sanitized rendering.
            let mut label_parts: Vec<String> = Vec::new();
            if let Some(label) = case_entry.and_then(|r| r.label.clone()) {
                label_parts.push(label);
            }
            for (param, &vi) in values_params.iter().zip(tuple) {
                let part = sanitize_value(&param.values[vi]);
                if !part.is_empty() {
                    label_parts.push(part);
                }
            }
            let suffix = if label_parts.is_empty() {
                format!("case_{index}")
            } else {
                format!("case_{index}_{}", label_parts.join("_"))
            };
            let case_name = format!("{func_name_str}::{suffix}");
            let static_ident = registration_ident(func_name_str, Some(&suffix));

            // Positional call: `#[case]` values from this row, `#[values]`
            // values from this combination, fixtures by name, and `ctx`.
            let case_values: &[Expr] = case_entry.map_or(&[], |r| r.values.as_slice());
            let call_args =
                build_call_args(&classes, case_values, values_params, tuple, has_fixtures);
            let inner = build_testcase_body(func_ident, &call_args, retry_on_error);
            let body = wrap_fixtures(&inner, &fixtures);
            registrations.push(quote! {
                #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_TEST_CASES)]
                #[linkme(crate = ::rigtest::__linkme)]
                static #static_ident: ::rigtest::registry::TestCase =
                    ::rigtest::registry::TestCase::new(
                        #case_name,
                        module_path!(),
                        file!(),
                        #serial,
                        #serial_group_tokens,
                        #timeout_tokens,
                        #no_timeout,
                        #retries_tokens,
                        #retry_on_error_set_tokens,
                        #tags_tokens,
                        |ctx| ::std::boxed::Box::pin(async move { #body }),
                    );
            });
        }
    }

    Ok(registrations)
}

/// Enumerate the cartesian product of value indices across `#[values]`
/// params, left-to-right with the last param varying fastest. Returns a
/// single empty tuple when there are no values params, so the caller's
/// case-row loop still runs once per case entry.
fn value_index_tuples(params: &[ValuesParam]) -> Vec<Vec<usize>> {
    let mut tuples: Vec<Vec<usize>> = vec![Vec::new()];
    for param in params {
        let mut next = Vec::with_capacity(tuples.len().saturating_mul(param.values.len()));
        for prefix in &tuples {
            for i in 0..param.values.len() {
                let mut combo = prefix.clone();
                combo.push(i);
                next.push(combo);
            }
        }
        tuples = next;
    }
    tuples
}

/// Render a value expression's tokens and keep only ASCII alphanumerics,
/// producing a name-safe label fragment (`"GET"` → `GET`, `200` → `200`,
/// `Method::Get` → `MethodGet`). Returns an empty string when nothing
/// survives, in which case the caller drops the fragment.
fn sanitize_value(expr: &Expr) -> String {
    quote! { #expr }
        .to_string()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

/// Generate the async body for the registered test wrapper. Without
/// `retry_on_error` the body is the historical single-line call. With a
/// matcher the body intercepts `Err(e)`, evaluates `matches!(&e, pat)`
/// against the user's typed error, and (when the pattern doesn't match)
/// wraps the boxed error in [`NotRetryEligible`][rigtest::NotRetryEligible]
/// so the subprocess runner can encode the retry-eligibility hint on the
/// wire. The user error is then boxed exactly as it always was.
fn build_testcase_body(
    func_ident: &syn::Ident,
    call_args: &[proc_macro2::TokenStream],
    retry_on_error: Option<&syn::Pat>,
) -> proc_macro2::TokenStream {
    if let Some(pat) = retry_on_error {
        quote! {
            match #func_ident(#(#call_args),*).await {
                ::core::result::Result::Ok(()) => ::core::result::Result::Ok(()),
                ::core::result::Result::Err(__rigtest_err) => {
                    let __rigtest_eligible = matches!(&__rigtest_err, #pat);
                    let __rigtest_boxed: ::std::boxed::Box<
                        dyn ::std::error::Error + ::std::marker::Send + ::std::marker::Sync,
                    > = ::std::boxed::Box::from(__rigtest_err);
                    let __rigtest_result: ::std::boxed::Box<
                        dyn ::std::error::Error + ::std::marker::Send + ::std::marker::Sync,
                    > = if __rigtest_eligible {
                        __rigtest_boxed
                    } else {
                        ::std::boxed::Box::new(
                            ::rigtest::NotRetryEligible(__rigtest_boxed),
                        )
                    };
                    ::core::result::Result::Err(__rigtest_result)
                }
            }
        }
    } else {
        quote! { #func_ident(#(#call_args),*).await }
    }
}

/// How a single `#[testcase]` parameter is wired into the generated wrapper.
enum ParamClass {
    /// The `ctx: Arc<TestContext>` argument.
    Ctx,
    /// A `#[case]`-tagged parameter receiving a per-row value.
    Case,
    /// A `#[values(...)]`-tagged parameter. Carries its index into the
    /// `values_params` slice so the per-combination value can be looked up.
    Values(usize),
    /// A fixture parameter, resolved by name against the same-named unit
    /// struct emitted by `#[fixture]`. Carries the parameter identifier.
    Fixture(syn::Ident),
}

/// Classify every parameter of a `#[testcase]` function.
///
/// A parameter is a `#[case]` value when its position is in
/// `case_positions`; otherwise it is the ctx argument when its type is
/// `Arc<…TestContext>` (at most one is permitted), and otherwise it is a
/// fixture argument named by its identifier. This keeps the historical
/// contract — a lone `Arc<TestContext>` parameter is always the ctx
/// argument, never a fixture.
fn classify_params(
    func: &ItemFn,
    case_positions: &[usize],
    values_params: &[ValuesParam],
) -> Result<Vec<ParamClass>, syn::Error> {
    let mut classes = Vec::with_capacity(func.sig.inputs.len());
    let mut ctx_seen = false;
    for (idx, input) in func.sig.inputs.iter().enumerate() {
        if case_positions.contains(&idx) {
            classes.push(ParamClass::Case);
            continue;
        }
        if let Some(vi) = values_params.iter().position(|p| p.position == idx) {
            classes.push(ParamClass::Values(vi));
            continue;
        }
        let FnArg::Typed(pat_type) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "#[testcase] functions cannot take a `self` parameter",
            ));
        };
        if type_is_arc_test_context(&pat_type.ty) {
            if ctx_seen {
                return Err(syn::Error::new_spanned(
                    input,
                    "#[testcase] accepts at most one `Arc<TestContext>` parameter",
                ));
            }
            ctx_seen = true;
            classes.push(ParamClass::Ctx);
        } else {
            let Pat::Ident(pat_ident) = pat_type.pat.as_ref() else {
                return Err(syn::Error::new_spanned(
                    &pat_type.pat,
                    "#[testcase] fixture parameter must be a plain identifier that names a \
                     #[fixture] in scope (patterns such as tuples or `_` are not supported)",
                ));
            };
            classes.push(ParamClass::Fixture(pat_ident.ident.clone()));
        }
    }
    Ok(classes)
}

/// Collect the fixture parameter identifiers in declaration order.
fn fixture_idents(classes: &[ParamClass]) -> Vec<syn::Ident> {
    classes
        .iter()
        .filter_map(|c| match c {
            ParamClass::Fixture(ident) => Some(ident.clone()),
            _ => None,
        })
        .collect()
}

/// Build the positional argument list for the call to the user's test
/// function. `#[case]` positions consume `case_values` left-to-right, the
/// ctx position becomes `ctx` (or a clone when fixtures are present, since
/// the wrapper still needs `ctx` for teardown), and fixture positions pass
/// the local bound by [`wrap_fixtures`].
fn build_call_args(
    classes: &[ParamClass],
    case_values: &[Expr],
    values_params: &[ValuesParam],
    tuple: &[usize],
    has_fixtures: bool,
) -> Vec<proc_macro2::TokenStream> {
    let ctx_arg = if has_fixtures {
        quote! { ::std::sync::Arc::clone(&ctx) }
    } else {
        quote! { ctx }
    };
    let mut case_iter = case_values.iter();
    classes
        .iter()
        .map(|class| match class {
            ParamClass::Ctx => ctx_arg.clone(),
            ParamClass::Fixture(ident) => quote! { #ident },
            ParamClass::Values(vi) => {
                let val = &values_params[*vi].values[tuple[*vi]];
                quote! { #val }
            }
            ParamClass::Case => {
                // Length is validated by `validate_case_shape`; a missing
                // value here would be an internal invariant break, so fall
                // back to a token that surfaces as a compile error rather
                // than silently dropping an argument.
                case_iter.next().map_or_else(
                    || quote! { compile_error!("internal error: case value count mismatch") },
                    |val| quote! { #val },
                )
            }
        })
        .collect()
}

/// Wrap `inner` (a `Result<(), BoxError>`-valued expression) in fixture
/// setup/teardown scopes. Fixtures are set up left-to-right and torn down
/// in LIFO order; a teardown error is surfaced only when the body
/// succeeded, otherwise the body's error wins. Setup uses `?`, so a fixture
/// whose setup fails aborts before the body runs and does not trigger
/// teardown of fixtures set up earlier.
fn wrap_fixtures(
    inner: &proc_macro2::TokenStream,
    fixtures: &[syn::Ident],
) -> proc_macro2::TokenStream {
    let mut acc = inner.clone();
    for ident in fixtures.iter().rev() {
        acc = quote! {{
            let #ident = #ident::__rigtest_fixture_setup(::std::sync::Arc::clone(&ctx)).await?;
            let __rigtest_body_result: ::core::result::Result<
                (),
                ::std::boxed::Box<dyn ::std::error::Error + ::std::marker::Send + ::std::marker::Sync>,
            > = { #acc };
            let __rigtest_teardown_result =
                #ident::__rigtest_fixture_teardown(::std::sync::Arc::clone(&ctx)).await;
            match __rigtest_body_result {
                ::core::result::Result::Ok(()) => {
                    __rigtest_teardown_result?;
                    ::core::result::Result::Ok(())
                }
                ::core::result::Result::Err(__rigtest_body_err) => {
                    ::core::result::Result::Err(__rigtest_body_err)
                }
            }
        }};
    }
    acc
}

/// Returns true when `ty` is `Arc<…TestContext>` — matched structurally by
/// an outer `Arc` whose sole type argument's path ends in `TestContext`.
fn type_is_arc_test_context(ty: &Type) -> bool {
    let Type::Path(tp) = ty else { return false };
    let Some(seg) = tp.path.segments.last() else {
        return false;
    };
    if seg.ident != "Arc" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return false;
    };
    args.args.iter().any(|arg| {
        let syn::GenericArgument::Type(Type::Path(inner)) = arg else {
            return false;
        };
        inner
            .path
            .segments
            .last()
            .is_some_and(|s| s.ident == "TestContext")
    })
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

/// Scan the function signature for `#[case]` and `#[values(...)]` parameter
/// markers, stripping them so the re-emitted function compiles, and return
/// the tagged `#[case]` positions plus the parsed `#[values]` dimensions.
/// A parameter tagged with both markers, or an empty `#[values()]`, is a
/// compile error.
fn collect_param_markers(func: &mut ItemFn) -> Result<(Vec<usize>, Vec<ValuesParam>), syn::Error> {
    let mut case_param_positions: Vec<usize> = Vec::new();
    let mut values_params: Vec<ValuesParam> = Vec::new();
    for (idx, input) in func.sig.inputs.iter_mut().enumerate() {
        let FnArg::Typed(pat_type) = input else {
            continue;
        };
        let mut is_case = false;
        let mut values: Option<Vec<Expr>> = None;
        let mut kept = Vec::with_capacity(pat_type.attrs.len());
        for a in pat_type.attrs.drain(..) {
            if a.path().is_ident("case") {
                is_case = true;
            } else if a.path().is_ident("values") {
                values = Some(parse_values_attr(&a)?);
            } else {
                kept.push(a);
            }
        }
        pat_type.attrs = kept;
        if is_case && values.is_some() {
            return Err(syn::Error::new(
                pat_type.span(),
                "a parameter cannot be tagged with both #[case] and #[values]; \
                 choose one dimension for it",
            ));
        }
        if is_case {
            case_param_positions.push(idx);
        }
        if let Some(values) = values {
            values_params.push(ValuesParam {
                position: idx,
                values,
            });
        }
    }
    Ok((case_param_positions, values_params))
}

/// A parameter tagged `#[values(expr, expr, ...)]`. Each such parameter is
/// one dimension of the cartesian product; the generated cases visit every
/// value in `values`.
struct ValuesParam {
    /// Positional index of the parameter in the function signature.
    position: usize,
    /// The value expressions listed in the attribute (guaranteed non-empty).
    values: Vec<Expr>,
}

/// Parse a `#[values(expr, expr, ...)]` attribute into its list of value
/// expressions. An empty list is a compile error.
fn parse_values_attr(attr: &syn::Attribute) -> Result<Vec<Expr>, syn::Error> {
    let values = attr
        .parse_args_with(Punctuated::<Expr, Token![,]>::parse_terminated)?
        .into_iter()
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Err(syn::Error::new(
            attr.span(),
            "#[values(...)] must list at least one value",
        ));
    }
    Ok(values)
}

/// Recognize `#[case(...)]` / `#[case::label(...)]` attributes and parse
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

/// Defines a function-scoped test fixture with automatic setup and optional
/// teardown, injected into `#[testcase]` functions by parameter name.
///
/// A fixture centralizes the "arrange" step of a test — resetting a
/// database, provisioning a temporary directory, seeding a queue — so many
/// tests can share it without repeating the code. Each test that names the
/// fixture as a parameter receives the fixture's returned value; the setup
/// runs just before the test body and the teardown just after.
///
/// # Signatures
///
/// The annotated function must be `async` and return `Result<T, E>`, where
/// `T` is the value injected into tests and `E` is any error type that
/// converts into the test's boxed error via `?` — the same constraint a
/// test body's error type satisfies. It takes either no parameters or a
/// single `ctx: Arc<TestContext>`:
///
/// ```text
/// async fn name() -> Result<T, E> { ... }
/// async fn name(ctx: Arc<TestContext>) -> Result<T, E> { ... }
/// ```
///
/// The `ctx` argument gives the fixture access to the global setup data and
/// the per-test lifecycle helpers, exactly as in a `#[testcase]`.
///
/// # Injection by parameter name
///
/// A test receives a fixture by declaring a parameter whose **name** matches
/// the fixture and whose type is the fixture's `T`. Any `#[testcase]`
/// parameter that is neither the `Arc<TestContext>` argument nor a `#[case]`
/// parameter is resolved as a fixture by name:
///
/// ```ignore
/// use std::sync::Arc;
/// use rigtest::{fixture, testcase, TestContext};
///
/// struct Db;
///
/// #[fixture]
/// async fn clean_db(_ctx: Arc<TestContext>) -> Result<Db, rigtest::Error> {
///     // reset state and hand back a handle
///     Ok(Db)
/// }
///
/// #[testcase]
/// async fn uses_db(_ctx: Arc<TestContext>, clean_db: Db) -> Result<(), rigtest::Error> {
///     // `clean_db` is the value returned by the fixture's setup
///     let _db = clean_db;
///     Ok(())
/// }
/// ```
///
/// Because a fixture resolves through an item named exactly like the
/// parameter, a test only needs that name in scope (a normal `use`), so
/// fixtures defined in one module work in tests in another.
///
/// # Teardown
///
/// Pass `teardown = <path>` to run cleanup after the test body. The teardown
/// function is `async fn(Arc<TestContext>) -> Result<(), E>` with the same
/// error type `E` as the fixture:
///
/// ```ignore
/// use std::sync::Arc;
/// use rigtest::{fixture, TestContext};
///
/// struct Db;
///
/// async fn drop_db(_ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
///     Ok(())
/// }
///
/// #[fixture(teardown = drop_db)]
/// async fn clean_db(_ctx: Arc<TestContext>) -> Result<Db, rigtest::Error> {
///     Ok(Db)
/// }
/// ```
///
/// When several fixtures are injected into one test they are set up
/// left-to-right and torn down in LIFO order (right-to-left). If the body
/// succeeds but a teardown fails, the teardown error fails the test; if the
/// body already failed, the body's error is reported and teardown errors are
/// ignored.
///
/// # v1 limitations
///
/// - Fixtures are **function-scoped**: setup and teardown run for every test
///   that names the fixture, once per test.
/// - The teardown function does **not** receive the fixture value.
/// - Teardown does **not** run when the test panics or is killed by a
///   `timeout` — the subprocess is terminated first. Use `#[global_teardown]`
///   for cleanup that must happen regardless of outcome.
/// - A fixture whose setup fails (returns `Err`) aborts the test before the
///   body runs; fixtures set up earlier in that test are not torn down.
/// - Fixtures cannot depend on other fixtures.
/// - A teardown failure on a test that also declares `retry_on_error` does
///   not pass through that matcher — it is treated as an ordinary
///   retry-eligible failure.
///
/// # Compile errors
///
/// The annotated function must be `async`, return `Result<T, E>`, and take
/// either no parameters or a single `Arc<TestContext>`. Other shapes are
/// rejected with an actionable message pointing at the offending span.
#[proc_macro_attribute]
pub fn fixture(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_fixture(attr, item) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_fixture(attr: TokenStream, item: TokenStream) -> Result<TokenStream, syn::Error> {
    let func: ItemFn = syn::parse(item)?;
    let teardown = parse_fixture_teardown(attr)?;

    if func.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            &func.sig,
            "#[fixture] functions must be `async`. Expected one of:\n  \
             async fn name() -> Result<T, E>\n  \
             async fn name(ctx: Arc<TestContext>) -> Result<T, E>",
        ));
    }

    let return_ty = match &func.sig.output {
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &func.sig,
                "#[fixture] functions must return `Result<T, E>` — `T` is the value \
                 injected into tests and `E` is the error type",
            ));
        }
        ReturnType::Type(_, ty) => ty.as_ref(),
    };
    let err_ty = result_err_type(return_ty).ok_or_else(|| {
        syn::Error::new_spanned(
            return_ty,
            "#[fixture] functions must return `Result<T, E>` — `T` is the value \
             injected into tests and `E` is the error type",
        )
    })?;

    // The fixture takes either no parameter or a single `Arc<TestContext>`.
    // The generated setup always accepts `ctx`; when the fixture declares
    // its own ctx parameter we re-emit it verbatim so the body's references
    // resolve, otherwise we supply an ignored one.
    let setup_param = match func.sig.inputs.len() {
        0 => quote! { _ctx: ::std::sync::Arc<::rigtest::TestContext> },
        1 => {
            let arg = func.sig.inputs.first().expect("len == 1");
            let FnArg::Typed(pat_type) = arg else {
                return Err(syn::Error::new_spanned(
                    arg,
                    "#[fixture] functions cannot take a `self` parameter. Expected:\n  \
                     async fn name(ctx: Arc<TestContext>) -> Result<T, E>",
                ));
            };
            if !type_is_arc_test_context(&pat_type.ty) {
                return Err(syn::Error::new_spanned(
                    &pat_type.ty,
                    "#[fixture] parameter must be `Arc<TestContext>`. Expected one of:\n  \
                     async fn name() -> Result<T, E>\n  \
                     async fn name(ctx: Arc<TestContext>) -> Result<T, E>",
                ));
            }
            quote! { #arg }
        }
        _ => {
            return Err(syn::Error::new_spanned(
                &func.sig.inputs,
                "#[fixture] functions accept at most one parameter. Expected one of:\n  \
                 async fn name() -> Result<T, E>\n  \
                 async fn name(ctx: Arc<TestContext>) -> Result<T, E>",
            ));
        }
    };

    let vis = &func.vis;
    let ident = &func.sig.ident;
    let body = &func.block;

    let teardown_fn = if let Some(path) = teardown {
        quote! {
            #vis async fn __rigtest_fixture_teardown(
                ctx: ::std::sync::Arc<::rigtest::TestContext>,
            ) -> ::core::result::Result<(), #err_ty> {
                #path(ctx).await
            }
        }
    } else {
        quote! {
            #[allow(clippy::unused_async)]
            #vis async fn __rigtest_fixture_teardown(
                _ctx: ::std::sync::Arc<::rigtest::TestContext>,
            ) -> ::core::result::Result<(), #err_ty> {
                ::core::result::Result::Ok(())
            }
        }
    };

    // Emit a non-unit (empty braced) struct rather than a unit struct: a
    // bare identifier that resolves to a *unit* struct is parsed as a
    // pattern, which would break both the test's fixture parameter binding
    // (`clean_db: Db`) and the wrapper's `let clean_db = …`. An empty braced
    // struct keeps `clean_db` usable as an ordinary binding while
    // `clean_db::__rigtest_fixture_setup` still resolves the associated fn.
    let expanded = quote! {
        #[allow(non_camel_case_types)]
        #[doc(hidden)]
        #vis struct #ident {}

        #[allow(non_camel_case_types)]
        impl #ident {
            #[allow(clippy::unused_async)]
            #vis async fn __rigtest_fixture_setup(#setup_param) -> #return_ty #body

            #teardown_fn
        }
    };
    Ok(TokenStream::from(expanded))
}

/// Parse the `#[fixture]` attribute arguments. The only accepted argument is
/// `teardown = <path>`, naming an `async fn(Arc<TestContext>) -> Result<(), E>`.
fn parse_fixture_teardown(attr: TokenStream) -> Result<Option<syn::Expr>, syn::Error> {
    let metas = Punctuated::<syn::Meta, Token![,]>::parse_terminated.parse(attr)?;
    let mut teardown: Option<syn::Expr> = None;
    for meta in &metas {
        match meta {
            syn::Meta::NameValue(nv) if nv.path.is_ident("teardown") => {
                teardown = Some(nv.value.clone());
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unknown argument for #[fixture]; the only supported argument is \
                     `teardown = <path to an async fn(Arc<TestContext>) -> Result<(), E>>`",
                ));
            }
        }
    }
    Ok(teardown)
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
/// # Signatures
///
/// `#[preflight]` accepts two signatures:
///
/// ```text
/// fn name() -> Preflight { ... }
/// fn name(env: &str) -> Preflight { ... }
/// ```
///
/// In the 1-arg form the framework passes the active profile name as a
/// `&str`, sourced from the `RIGTEST_PROFILE` environment variable
/// (defaulting to the empty string when unset). The parameter type must
/// be exactly `&str` — `String`, `&String`, `Cow<'_, str>`, and
/// `&mut str` are rejected at compile time.
///
/// `async fn`, more than one parameter, and return types other than
/// `Preflight` are rejected with actionable messages.
///
/// # Examples
///
/// 0-arg form:
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
///
/// 1-arg form branching on profile:
///
/// ```ignore
/// use rigtest::Preflight;
///
/// #[rigtest::preflight]
/// fn preflight(env: &str) -> Preflight {
///     match env {
///         "prod" => Preflight::new().http("api", "https://api.prod.example.com/health"),
///         _ => Preflight::new().http("api", "https://api.staging.example.com/health"),
///     }
/// }
/// ```
///
/// # Rejected shapes
///
/// The following shapes are rejected at compile time with an actionable
/// message: parameter types other than exactly `&str` (`String`,
/// `&String`, `&mut str`, `Cow<'_, str>`); more than one parameter; an
/// `async fn`; a missing or non-`Preflight` return type.
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
             builder function only constructs the Preflight description). Expected one of:\n  \
             fn name() -> Preflight\n  fn name(env: &str) -> Preflight",
        ));
    }

    // Accept exactly 0 or 1 parameters. The 1-arg form must be `&str`
    // (exact match — `String`, `&String`, `Cow<'_, str>`, `&mut str` are
    // rejected so a slip in the signature does not silently bind the wrong
    // type to the active profile name).
    let takes_profile = match func.sig.inputs.len() {
        0 => false,
        1 => {
            let arg = func.sig.inputs.first().expect("len == 1");
            validate_preflight_param(arg)?;
            true
        }
        _ => {
            return Err(syn::Error::new_spanned(
                &func.sig.inputs,
                "#[preflight] functions accept at most one parameter. Expected one of:\n  \
                 fn name() -> Preflight\n  fn name(env: &str) -> Preflight",
            ));
        }
    };

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

    // The registry stores `fn(&str) -> Preflight`. For the 0-arg form we
    // emit a thin adapter that discards the profile argument; for the
    // 1-arg form we register the user's function directly.
    let adapter = if takes_profile {
        quote! { #func_ident }
    } else {
        quote! { (|_profile: &::core::primitive::str| #func_ident()) as fn(&::core::primitive::str) -> ::rigtest::Preflight }
    };

    let expanded = quote! {
        #func

        #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_PREFLIGHT)]
        #[linkme(crate = ::rigtest::__linkme)]
        static #static_ident: ::rigtest::registry::PreflightEntry =
            ::rigtest::registry::PreflightEntry::new(#adapter);
    };
    Ok(TokenStream::from(expanded))
}

/// Validate that the single parameter on a 1-arg `#[preflight]` is exactly
/// `&str` — not `&mut str`, `String`, `&String`, `Cow<'_, str>`, or anything
/// else.
fn validate_preflight_param(arg: &FnArg) -> Result<(), syn::Error> {
    let FnArg::Typed(pat_type) = arg else {
        return Err(syn::Error::new_spanned(
            arg,
            "#[preflight] functions must not have a `self` parameter. Expected:\n  \
             fn name(env: &str) -> Preflight",
        ));
    };
    if param_is_str_ref(&pat_type.ty) {
        Ok(())
    } else {
        Err(syn::Error::new_spanned(
            &pat_type.ty,
            "#[preflight] parameter must be `&str` exactly (not `String`, `&String`, \
             `&mut str`, or `Cow<'_, str>`). Expected one of:\n  \
             fn name() -> Preflight\n  fn name(env: &str) -> Preflight",
        ))
    }
}

/// Returns true when `ty` is `&str` (with any or no lifetime, shared
/// reference only). Rejects `&mut str`, `String`, `Cow<'_, str>`, and any
/// other shape so a typo cannot silently bind a different type to the
/// active profile name.
fn param_is_str_ref(ty: &Type) -> bool {
    let Type::Reference(r) = ty else {
        return false;
    };
    if r.mutability.is_some() {
        return false;
    }
    let Type::Path(tp) = r.elem.as_ref() else {
        return false;
    };
    if tp.qself.is_some() {
        return false;
    }
    tp.path.get_ident().is_some_and(|ident| ident == "str")
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
