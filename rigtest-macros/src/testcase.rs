//! Parse → plan → generate pipeline for the `#[testcase]` attribute macro.
//!
//! The public entry point is [`expand`], which operates entirely on
//! `proc_macro2`/`syn` types so every stage is unit-testable without a
//! `proc_macro` bridge. The three stages are:
//!
//! - **parse** ([`parse_spec`], [`extract_case_rows`], [`classify_params`]) —
//!   turn the attribute tokens, stacked `#[case(...)]` rows, and function
//!   parameters into parsed values ([`TestcaseSpec`], [`CaseRow`],
//!   [`ParamKind`]). All validation and its exact error messages live here.
//! - **plan** ([`plan`]) — a pure function computing the whole expansion as
//!   data: the cartesian product of cases, each carrying its final name,
//!   registration ident, and per-parameter argument source.
//! - **generate** ([`generate`]) — a mechanical `quote!` walk over the plan
//!   emitting the re-emitted function plus one registration static per case.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::Expr;
use syn::FnArg;
use syn::ItemFn;
use syn::LitStr;
use syn::Pat;
use syn::ReturnType;
use syn::Token;
use syn::Type;

use crate::{result_err_type, type_is_arc_test_context};

/// Convert the attribute and item token streams into the expanded output,
/// or a `syn::Error` carrying the compile diagnostic. All logic operates on
/// `proc_macro2`/`syn` types so it is unit-testable.
pub fn expand(attr: TokenStream, item: TokenStream) -> Result<TokenStream, syn::Error> {
    let mut func: ItemFn = syn::parse2(item)?;
    let func_name_str = func.sig.ident.to_string();

    let spec = parse_spec(attr)?;
    if spec.retry_on_error.is_some() {
        validate_retry_on_error_signature(&func)?;
    }

    // Extract and strip stacked `#[case(...)]` / `#[case::label(...)]`
    // attributes from the function. Anything else stays on the re-emitted
    // function definition.
    let case_rows = extract_case_rows(&mut func)?;

    // Classify every parameter in a single pass, stripping the `#[case]` /
    // `#[values(...)]` markers so the re-emitted function compiles unchanged.
    let params = classify_params(&mut func)?;

    validate_case_shape(&func, &case_rows, &params)?;

    let plan = plan(spec, &params, &case_rows, &func_name_str);
    Ok(generate(&func, &plan))
}

// ---------------------------------------------------------------------------
// Parse
// ---------------------------------------------------------------------------

/// How the `serial` flag was declared.
#[derive(Default)]
enum SerialMode {
    /// No `serial` flag.
    #[default]
    None,
    /// `serial` — fully exclusive.
    Exclusive,
    /// `serial = "group"` — named serial group.
    Group(LitStr),
}

/// The parsed `#[testcase(...)]` flags, as values rather than pre-rendered
/// tokens. Rendering to the `TestCase::new` argument list happens in
/// [`generate`].
#[derive(Default)]
struct TestcaseSpec {
    serial: SerialMode,
    timeout: Option<Expr>,
    no_timeout: bool,
    retries: Option<Expr>,
    /// The user-supplied pattern from `retry_on_error = <pat>`, if present.
    retry_on_error: Option<Pat>,
    tags: Vec<LitStr>,
}

/// Parse the `#[testcase(...)]` attribute arguments into a [`TestcaseSpec`],
/// preserving every validation and its exact error message.
fn parse_spec(attr: TokenStream) -> Result<TestcaseSpec, syn::Error> {
    let metas = Punctuated::<syn::Meta, Token![,]>::parse_terminated
        .parse2(attr)
        .unwrap_or_default();
    let mut spec = TestcaseSpec::default();
    let mut timeout_set = false;
    for meta in &metas {
        match meta {
            syn::Meta::Path(p) if p.is_ident("serial") => spec.serial = SerialMode::Exclusive,
            syn::Meta::NameValue(nv) if nv.path.is_ident("serial") => {
                spec.serial = SerialMode::Group(parse_serial_group(&nv.value)?);
            }
            syn::Meta::Path(p) if p.is_ident("no_timeout") => spec.no_timeout = true,
            syn::Meta::NameValue(nv) if nv.path.is_ident("timeout") => {
                timeout_set = true;
                spec.timeout = Some(nv.value.clone());
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("retries") => {
                spec.retries = Some(nv.value.clone());
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("retry_on_error") => {
                spec.retry_on_error = Some(parse_retry_on_error_pattern(&nv.value)?);
            }
            syn::Meta::NameValue(nv) if nv.path.is_ident("tags") => {
                spec.tags = parse_tags(&nv.value)?;
            }
            _ => {}
        }
    }
    if spec.no_timeout && timeout_set {
        return Err(syn::Error::new(
            Span::call_site(),
            "#[testcase]: `no_timeout` cannot be combined with `timeout = …`; \
             `no_timeout` forces no timeout (opting out of any suite-wide default), \
             while `timeout` sets an explicit one — pick one",
        ));
    }
    Ok(spec)
}

/// Parse the value of `serial = "group"` as a string literal naming the
/// serial group. A non-string value is rejected with a compile error so a
/// typo like `serial = db` fails loudly rather than silently.
fn parse_serial_group(value: &Expr) -> syn::Result<LitStr> {
    if let Expr::Lit(syn::ExprLit {
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
fn parse_retry_on_error_pattern(value: &Expr) -> syn::Result<Pat> {
    let tokens = quote! { #value };
    syn::parse::Parser::parse2(Pat::parse_multi_with_leading_vert, tokens).map_err(|e| {
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

/// Parse the value of `tags = [...]` into the list of tag literals.
///
/// Accepts an array literal of string literals. Each tag must be a non-empty
/// string with no whitespace — both are runner-side concerns surfaced as a
/// compile error so a typo in a tag does not silently match nothing at
/// runtime.
fn parse_tags(value: &Expr) -> syn::Result<Vec<LitStr>> {
    let Expr::Array(array) = value else {
        return Err(syn::Error::new_spanned(
            value,
            "`tags` must be an array literal of string literals, e.g. tags = [\"smoke\", \"regression\"]",
        ));
    };

    let mut literals: Vec<LitStr> = Vec::with_capacity(array.elems.len());
    for elem in &array.elems {
        let lit = match elem {
            Expr::Lit(syn::ExprLit {
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

    Ok(literals)
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

/// Drain the stacked `#[case(...)]` / `#[case::label(...)]` attributes off
/// the function, leaving unrelated attributes in place, and return the
/// parsed rows.
fn extract_case_rows(func: &mut ItemFn) -> Result<Vec<CaseRow>, syn::Error> {
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
    Ok(case_rows)
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

/// How a single `#[testcase]` parameter is wired into the generated wrapper.
/// Each variant owns the data needed to resolve the parameter, so no side
/// slice is consulted during planning.
enum ParamKind {
    /// The `ctx: Arc<TestContext>` argument.
    Ctx,
    /// A `#[case]`-tagged parameter receiving a per-row value.
    Case,
    /// A `#[values(...)]`-tagged parameter, owning its value expressions.
    Values(Vec<Expr>),
    /// A fixture parameter, resolved by name against the same-named struct
    /// emitted by `#[fixture]`. Carries the parameter identifier.
    Fixture(syn::Ident),
}

/// Classify every parameter of a `#[testcase]` function in a single pass,
/// stripping the `#[case]` / `#[values(...)]` markers as it goes.
///
/// A parameter tagged `#[case]` is a case value; tagged `#[values(...)]` a
/// values dimension (empty list rejected); tagging both is a compile error.
/// Otherwise it is the ctx argument when its type is `Arc<…TestContext>`
/// (at most one permitted), else a fixture named by its identifier. This
/// keeps the historical contract — a lone `Arc<TestContext>` parameter is
/// always the ctx argument, never a fixture.
fn classify_params(func: &mut ItemFn) -> Result<Vec<ParamKind>, syn::Error> {
    let mut params = Vec::with_capacity(func.sig.inputs.len());
    let mut ctx_seen = false;
    for input in &mut func.sig.inputs {
        let FnArg::Typed(pat_type) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "#[testcase] functions cannot take a `self` parameter",
            ));
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
            params.push(ParamKind::Case);
            continue;
        }
        if let Some(values) = values {
            params.push(ParamKind::Values(values));
            continue;
        }

        if type_is_arc_test_context(&pat_type.ty) {
            if ctx_seen {
                return Err(syn::Error::new_spanned(
                    &*pat_type,
                    "#[testcase] accepts at most one `Arc<TestContext>` parameter",
                ));
            }
            ctx_seen = true;
            params.push(ParamKind::Ctx);
        } else {
            let Pat::Ident(pat_ident) = pat_type.pat.as_ref() else {
                return Err(syn::Error::new_spanned(
                    &pat_type.pat,
                    "#[testcase] fixture parameter must be a plain identifier that names a \
                     #[fixture] in scope (patterns such as tuples or `_` are not supported)",
                ));
            };
            params.push(ParamKind::Fixture(pat_ident.ident.clone()));
        }
    }
    Ok(params)
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

/// Validate the relationship between `#[case(...)]` rows and `#[case]`
/// parameter markers, surfacing mismatches as actionable compile errors
/// pointing at the offending span.
fn validate_case_shape(
    func: &ItemFn,
    case_rows: &[CaseRow],
    params: &[ParamKind],
) -> Result<(), syn::Error> {
    let case_positions: Vec<usize> = params
        .iter()
        .enumerate()
        .filter_map(|(i, p)| matches!(p, ParamKind::Case).then_some(i))
        .collect();

    if !case_rows.is_empty() && case_positions.is_empty() {
        return Err(syn::Error::new(
            case_rows[0].span,
            "#[case(...)] rows are present but no function parameter is tagged with #[case]; \
             add `#[case]` to each parameter that should receive a per-row value",
        ));
    }
    if case_rows.is_empty() && !case_positions.is_empty() {
        let span = func
            .sig
            .inputs
            .iter()
            .nth(case_positions[0])
            .map_or_else(Span::call_site, Spanned::span);
        return Err(syn::Error::new(
            span,
            "function parameter is tagged with #[case] but no #[case(...)] rows are stacked \
             above the function; add one or more `#[case(value, ...)]` attributes",
        ));
    }
    for row in case_rows {
        if row.values.len() != case_positions.len() {
            return Err(syn::Error::new(
                row.span,
                format!(
                    "#[case(...)] has {got} value(s) but the function has {want} #[case]-tagged \
                     parameter(s); every row must supply exactly one value per tagged parameter",
                    got = row.values.len(),
                    want = case_positions.len(),
                ),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// The resolved source for a single positional argument in the call to the
/// user's test function.
enum ArgSource {
    /// The ctx argument. Rendered as a bare `ctx` when no fixtures are
    /// present, or `Arc::clone(&ctx)` when they are (the wrapper keeps `ctx`
    /// for teardown).
    Ctx,
    /// A `#[case]` value for this row.
    Case(Expr),
    /// The chosen `#[values]` value for this combination.
    Values(Expr),
    /// A fixture, passed by the local bound by `wrap_fixtures`.
    Fixture(syn::Ident),
}

/// A single generated case: its registered name, registration static ident,
/// and the per-parameter argument sources in declaration order.
struct GeneratedCase {
    name: String,
    static_ident: syn::Ident,
    args: Vec<ArgSource>,
}

/// The complete expansion as data: the shared flags, the fixture idents for
/// `wrap_fixtures`, and one [`GeneratedCase`] per combination.
struct TestcasePlan {
    spec: TestcaseSpec,
    fixtures: Vec<syn::Ident>,
    cases: Vec<GeneratedCase>,
}

/// Compute the [`TestcasePlan`] from the parsed spec, classified parameters,
/// and case rows. Pure — no token emission — so it can be exercised directly
/// by unit tests.
///
/// The cartesian product visits the case-rows dimension outermost (a single
/// implicit empty entry when there are no `#[case]` rows), then each
/// `#[values]` parameter left-to-right with the last varying fastest. When
/// there are neither `#[case]` rows nor `#[values]` parameters, a single
/// case is emitted under the historical unsuffixed registration name.
fn plan(
    spec: TestcaseSpec,
    params: &[ParamKind],
    case_rows: &[CaseRow],
    func_name_str: &str,
) -> TestcasePlan {
    let fixtures = fixture_idents(params);

    // The `#[values]` value lists, in declaration order.
    let values: Vec<&[Expr]> = params
        .iter()
        .filter_map(|p| match p {
            ParamKind::Values(v) => Some(v.as_slice()),
            _ => None,
        })
        .collect();

    // No `#[case]`/`#[values]` dimensions → single test under the historical
    // unsuffixed registration name.
    if case_rows.is_empty() && values.is_empty() {
        let args = params
            .iter()
            .map(|p| match p {
                ParamKind::Fixture(ident) => ArgSource::Fixture(ident.clone()),
                // No `#[case]`/`#[values]` params exist in this branch.
                _ => ArgSource::Ctx,
            })
            .collect();
        return TestcasePlan {
            spec,
            fixtures,
            cases: vec![GeneratedCase {
                name: func_name_str.to_string(),
                static_ident: registration_ident(func_name_str, None),
                args,
            }],
        };
    }

    let case_entries: Vec<Option<&CaseRow>> = if case_rows.is_empty() {
        vec![None]
    } else {
        case_rows.iter().map(Some).collect()
    };
    let value_tuples = value_index_tuples(&values);

    let mut cases = Vec::with_capacity(case_entries.len().saturating_mul(value_tuples.len()));
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
            for (vals, &vi) in values.iter().zip(tuple) {
                let part = sanitize_value(&vals[vi]);
                if !part.is_empty() {
                    label_parts.push(part);
                }
            }
            let suffix = if label_parts.is_empty() {
                format!("case_{index}")
            } else {
                format!("case_{index}_{}", label_parts.join("_"))
            };
            let name = format!("{func_name_str}::{suffix}");
            let static_ident = registration_ident(func_name_str, Some(&suffix));

            // Positional argument sources: `#[case]` values from this row,
            // `#[values]` values from this combination, fixtures by name,
            // and `ctx`.
            let case_values: &[Expr] = case_entry.map_or(&[], |r| r.values.as_slice());
            let mut case_iter = case_values.iter();
            let mut values_dim = 0usize;
            let args = params
                .iter()
                .map(|p| match p {
                    ParamKind::Ctx => ArgSource::Ctx,
                    ParamKind::Fixture(ident) => ArgSource::Fixture(ident.clone()),
                    ParamKind::Values(v) => {
                        let vi = tuple[values_dim];
                        values_dim += 1;
                        ArgSource::Values(v[vi].clone())
                    }
                    // Arity is guaranteed by `validate_case_shape`.
                    ParamKind::Case => ArgSource::Case(
                        case_iter
                            .next()
                            .cloned()
                            .expect("case value count validated by validate_case_shape"),
                    ),
                })
                .collect();

            cases.push(GeneratedCase {
                name,
                static_ident,
                args,
            });
        }
    }

    TestcasePlan {
        spec,
        fixtures,
        cases,
    }
}

/// Collect the fixture parameter identifiers in declaration order.
fn fixture_idents(params: &[ParamKind]) -> Vec<syn::Ident> {
    params
        .iter()
        .filter_map(|p| match p {
            ParamKind::Fixture(ident) => Some(ident.clone()),
            _ => None,
        })
        .collect()
}

/// Enumerate the cartesian product of value indices across `#[values]`
/// params, left-to-right with the last param varying fastest. Returns a
/// single empty tuple when there are no values params, so the caller's
/// case-row loop still runs once per case entry.
fn value_index_tuples(values: &[&[Expr]]) -> Vec<Vec<usize>> {
    let mut tuples: Vec<Vec<usize>> = vec![Vec::new()];
    for dim in values {
        let mut next = Vec::with_capacity(tuples.len().saturating_mul(dim.len()));
        for prefix in &tuples {
            for i in 0..dim.len() {
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

/// Build the registration static ident from the function name and an
/// optional case suffix.
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

// ---------------------------------------------------------------------------
// Generate
// ---------------------------------------------------------------------------

/// Emit the re-emitted function plus one registration static per generated
/// case. A single mechanical `quote!` walk over the plan — the only token
/// emission path.
fn generate(func: &ItemFn, plan: &TestcasePlan) -> TokenStream {
    let func_ident = &func.sig.ident;
    let has_fixtures = !plan.fixtures.is_empty();

    let serial = matches!(plan.spec.serial, SerialMode::Exclusive);
    let serial_group = if let SerialMode::Group(lit) = &plan.spec.serial {
        quote! { Some(#lit) }
    } else {
        quote! { None }
    };
    let timeout = plan
        .spec
        .timeout
        .as_ref()
        .map_or_else(|| quote! { None }, |expr| quote! { Some(#expr) });
    let no_timeout = plan.spec.no_timeout;
    let retries = plan
        .spec
        .retries
        .as_ref()
        .map_or_else(|| quote! { 0u32 }, |expr| quote! { #expr });
    let retry_on_error_set = plan.spec.retry_on_error.is_some();
    let tags = &plan.spec.tags;
    let tags = quote! { &[ #( #tags ),* ] as &'static [&'static str] };

    let registrations = plan.cases.iter().map(|case| {
        let call_args: Vec<TokenStream> = case
            .args
            .iter()
            .map(|arg| match arg {
                ArgSource::Ctx => {
                    if has_fixtures {
                        quote! { ::std::sync::Arc::clone(&ctx) }
                    } else {
                        quote! { ctx }
                    }
                }
                ArgSource::Case(expr) | ArgSource::Values(expr) => quote! { #expr },
                ArgSource::Fixture(ident) => quote! { #ident },
            })
            .collect();
        let inner = build_testcase_body(func_ident, &call_args, plan.spec.retry_on_error.as_ref());
        let body = wrap_fixtures(&inner, &plan.fixtures);
        let name = &case.name;
        let static_ident = &case.static_ident;
        quote! {
            #[::rigtest::__linkme::distributed_slice(::rigtest::registry::RIG_TEST_CASES)]
            #[linkme(crate = ::rigtest::__linkme)]
            static #static_ident: ::rigtest::registry::TestCase =
                ::rigtest::registry::TestCase::new(
                    #name,
                    module_path!(),
                    file!(),
                    #serial,
                    #serial_group,
                    #timeout,
                    #no_timeout,
                    #retries,
                    #retry_on_error_set,
                    #tags,
                    |ctx| ::std::boxed::Box::pin(async move { #body }),
                );
        }
    });

    quote! {
        #[allow(clippy::unused_async)]
        #func

        #(#registrations)*
    }
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
    call_args: &[TokenStream],
    retry_on_error: Option<&Pat>,
) -> TokenStream {
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

/// Wrap `inner` (a `Result<(), BoxError>`-valued expression) in fixture
/// setup/teardown scopes. Fixtures are set up left-to-right and torn down
/// in LIFO order; a teardown error is surfaced only when the body
/// succeeded, otherwise the body's error wins. Setup uses `?`, so a fixture
/// whose setup fails aborts before the body runs and does not trigger
/// teardown of fixtures set up earlier.
fn wrap_fixtures(inner: &TokenStream, fixtures: &[syn::Ident]) -> TokenStream {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse an attribute + function pair through the parse and plan stages.
    fn plan_of(attr: &str, item: &str) -> TestcasePlan {
        let attr: TokenStream = attr.parse().expect("attr tokens");
        let mut func: ItemFn = syn::parse_str(item).expect("parse fn");
        let func_name_str = func.sig.ident.to_string();
        let spec = parse_spec(attr).expect("parse spec");
        let case_rows = extract_case_rows(&mut func).expect("case rows");
        let params = classify_params(&mut func).expect("classify");
        validate_case_shape(&func, &case_rows, &params).expect("shape");
        plan(spec, &params, &case_rows, &func_name_str)
    }

    #[test]
    fn single_case_is_unsuffixed_with_bare_ctx() {
        let plan = plan_of(
            "",
            "async fn plain(_ctx: Arc<TestContext>) -> Result<(), E> { Ok(()) }",
        );
        assert_eq!(plan.cases.len(), 1);
        let case = &plan.cases[0];
        // Historical unsuffixed registration name.
        assert_eq!(case.name, "plain");
        assert_eq!(case.static_ident.to_string(), "__RIGTEST_TESTCASE_PLAIN");
        // The Q5 guard: one ctx arg source, no fixtures → bare `ctx`.
        assert!(plan.fixtures.is_empty());
        assert_eq!(case.args.len(), 1);
        assert!(matches!(case.args[0], ArgSource::Ctx));
    }

    #[test]
    fn values_product_names_and_sources() {
        let plan = plan_of(
            "",
            "async fn method_status(\
                _ctx: Arc<TestContext>,\
                #[values(\"GET\", \"POST\")] method: &str,\
                #[values(200, 404)] status: u16,\
             ) -> Result<(), E> { Ok(()) }",
        );
        let names: Vec<&str> = plan.cases.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "method_status::case_1_GET_200",
                "method_status::case_2_GET_404",
                "method_status::case_3_POST_200",
                "method_status::case_4_POST_404",
            ]
        );
        // Per-param value sources for the first combination: ctx, "GET", 200.
        let render = |a: &ArgSource| match a {
            ArgSource::Values(e) => quote! { #e }.to_string(),
            ArgSource::Ctx => "ctx".to_string(),
            _ => "other".to_string(),
        };
        let first: Vec<String> = plan.cases[0].args.iter().map(render).collect();
        assert_eq!(first, vec!["ctx", "\"GET\"", "200"]);
        let last: Vec<String> = plan.cases[3].args.iter().map(render).collect();
        assert_eq!(last, vec!["ctx", "\"POST\"", "404"]);
    }

    #[test]
    fn case_times_values_product() {
        let plan = plan_of(
            "",
            "#[case::lo(1u32)]\
             #[case::hi(10u32)]\
             async fn ct(\
                _ctx: Arc<TestContext>,\
                #[case] base: u32,\
                #[values(\"x\", \"y\")] tag: &str,\
             ) -> Result<(), E> { Ok(()) }",
        );
        let names: Vec<&str> = plan.cases.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "ct::case_1_lo_x",
                "ct::case_2_lo_y",
                "ct::case_3_hi_x",
                "ct::case_4_hi_y",
            ]
        );
        // case_2 (lo, y): ctx, case value 1u32, values value "y".
        let args = &plan.cases[1].args;
        assert!(matches!(args[0], ArgSource::Ctx));
        assert!(matches!(&args[1], ArgSource::Case(e) if quote!{#e}.to_string() == "1u32"));
        assert!(matches!(&args[2], ArgSource::Values(e) if quote!{#e}.to_string() == "\"y\""));
    }

    #[test]
    fn fixture_param_is_classified_and_listed() {
        let plan = plan_of(
            "",
            "async fn f(_ctx: Arc<TestContext>, answer: u32) -> Result<(), E> { Ok(()) }",
        );
        assert_eq!(plan.fixtures.len(), 1);
        assert_eq!(plan.fixtures[0].to_string(), "answer");
        let case = &plan.cases[0];
        // Unsuffixed single case, fixture arg source names the fixture.
        assert_eq!(case.name, "f");
        assert!(matches!(&case.args[1], ArgSource::Fixture(id) if id == "answer"));
    }

    #[test]
    fn flag_parsing() {
        let group = parse_spec("serial = \"db\"".parse().unwrap()).unwrap();
        assert!(matches!(group.serial, SerialMode::Group(ref s) if s.value() == "db"));

        let nt = parse_spec("no_timeout".parse().unwrap()).unwrap();
        assert!(nt.no_timeout);
        assert!(nt.timeout.is_none());

        let to = parse_spec("timeout = Duration::from_secs(5)".parse().unwrap()).unwrap();
        assert!(to.timeout.is_some());
        assert!(!to.no_timeout);

        // Conflict is still rejected.
        assert!(parse_spec("no_timeout, timeout = D".parse().unwrap()).is_err());
    }

    /// Render a full invocation through parse → plan → generate, returning the
    /// emitted tokens as a string.
    fn render_of(attr: &str, item: &str) -> String {
        let attr: TokenStream = attr.parse().expect("attr tokens");
        let mut func: ItemFn = syn::parse_str(item).expect("parse fn");
        let func_name_str = func.sig.ident.to_string();
        let spec = parse_spec(attr).expect("parse spec");
        let case_rows = extract_case_rows(&mut func).expect("case rows");
        let params = classify_params(&mut func).expect("classify");
        validate_case_shape(&func, &case_rows, &params).expect("shape");
        let plan = plan(spec, &params, &case_rows, &func_name_str);
        generate(&func, &plan).to_string()
    }

    #[test]
    fn generate_clones_ctx_only_with_fixtures() {
        // No fixtures: ctx is passed bare — `Arc::clone` appears nowhere in the
        // emitted tokens. Locks the single-case bare-ctx invariant at the
        // token-rendering layer (not just the ArgSource value).
        let plain = render_of(
            "",
            "async fn plain(ctx: Arc<TestContext>) -> Result<(), E> { Ok(()) }",
        );
        assert!(
            !plain.contains("Arc :: clone"),
            "bare ctx expected without fixtures, got: {plain}"
        );

        // With a fixture present: `Arc::clone(&ctx)` is emitted (the ctx arg
        // clone plus the wrap_fixtures setup/teardown calls).
        let with_fixture = render_of(
            "",
            "async fn uses_fix(ctx: Arc<TestContext>, db: Db) -> Result<(), E> { Ok(()) }",
        );
        assert!(
            with_fixture.contains("Arc :: clone"),
            "expected Arc::clone when a fixture is present, got: {with_fixture}"
        );
    }
}
