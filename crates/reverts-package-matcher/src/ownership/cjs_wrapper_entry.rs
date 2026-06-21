//! Externalize self-contained esbuild `__commonJS` package-entry thunks via
//! whole-namespace passthrough.
//!
//! esbuild inlines a CommonJS package as an anonymous module
//! `var <NS> = <helper>((<exports>[, <module>]) => { … <exports>.member = … })`
//! whose single exported binding is the memoized thunk `<NS>`. Consumers read
//! the package only through that namespace (`<NS>().member`), never the thunk's
//! internal bindings (they are sealed in the factory closure). `--minify`
//! strips the public `__export({member: …})` surface map, so the per-member
//! externalization proof in [`super::importable`] cannot fire (the module
//! exports the thunk binding, not the package's public members). But because
//! consumers go through the namespace, no per-member mapping is needed: when
//! such a thunk IS the package, the whole body can be replaced by
//! `import * as ns from "<pkg>"` with the thunk returning the namespace.
//! Replacing the entire factory body severs whatever the body referenced, so
//! the thunk's own outgoing module dependencies do not constrain this — only
//! that consumers reach the package solely through `<NS>()` (guaranteed by the
//! sealed factory closure) and that the thunk really IS the package (the
//! identity gate below).
//!
//! We encode that as a semantic-path proof on the thunk's package. The
//! planner's existing `CommonJsWrapper` external-package adapter already turns
//! such an attribution into the bare import + namespace passthrough
//! (`function <thunk>() { return <ns>; }`) and drops the body (see planner test
//! `anonymous_bundle_external_attribution_uses_external_adapter`), so this pass
//! needs no planner change.
//!
//! ## Identity gate (why function-match count is not enough)
//!
//! The matcher attributes a thunk to a package by aggregate function-signature
//! similarity, which is PROMISCUOUS across packages — e.g. cc-2.1.89's ajv
//! codegen thunk (`<exports>.regexpCode=…, <exports>.str=…`) function-matches
//! react with enough signatures to look like react. Externalizing it as react
//! would emit `import … from "react"` and break at runtime (`react.Name` is not
//! a constructor). So trusting `package_name` is unsafe. The airtight gate here
//! is PUBLIC-SURFACE IDENTITY: the thunk must assign the matched package's own
//! public export names onto its exports object (`<exports>.<member>=`). A real
//! react entry assigns `useState`/`useEffect`/… (react's public members); the
//! ajv thunk assigns none of react's members and is rejected.

use std::collections::{BTreeMap, BTreeSet};

use oxc_allocator::Allocator;
use oxc_ast::Visit;
use oxc_ast::ast::{AssignmentExpression, AssignmentTarget, Expression};
use oxc_ast::visit::walk::walk_assignment_expression;
use oxc_parser::Parser;
use oxc_span::SourceType;
use reverts_input::InputRows;

use crate::source::ast_export_helpers::object_expression_static_keys;

use crate::pipeline::is_anonymous_bundle_package_candidate;
use crate::{
    PackageSource, VersionedPackageMatchReport, accepted_external_modules,
    package_source_exported_members, package_source_public_export_proofs,
};

use super::promotion::{ExternalImportPromotion, apply_external_import_promotion};

/// Minimum number of the matched package's PUBLIC export names that must appear
/// as `<exports>.<member>=` assignments in the thunk body. This is the identity
/// proof — a coincidental function-shape match to the wrong package assigns
/// none of that package's members, so a non-trivial floor rejects it. Kept
/// modest so small packages (picomatch) still qualify, but >1 so a single
/// accidental name collision cannot pass.
const MIN_PUBLIC_MEMBER_ASSIGNMENTS: usize = 4;

/// Each package version's ROOT PUBLIC-API member names — the true
/// `require("pkg")` / `import "pkg"` surface, NOT the union of every file's
/// exports (so an internal sub-module's own members, e.g. semver/internal/re's
/// `COMPARATOR`, never leak in). Combines: (a) re-export indirection
/// (react index.js → cjs/react.js) via `package_source_public_export_proofs`;
/// (b) object-literal aggregation (semver `module.exports = {valid, …}`) via the
/// ROOT source's exported names; (f) function-export-with-attached-methods
/// (picomatch) via form-(f)-only extraction over all sources.
pub(crate) fn package_public_member_universe(
    package_sources: &[PackageSource],
) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut members_by_package = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for proof in package_source_public_export_proofs(package_sources) {
        if proof.export_specifier == proof.package_name {
            members_by_package
                .entry((proof.package_name, proof.package_version))
                .or_default()
                .extend(proof.public_members);
        }
    }
    for source in package_sources {
        if source.export_specifier == source.package_name {
            members_by_package
                .entry((source.package_name.clone(), source.package_version.clone()))
                .or_default()
                .extend(package_source_exported_members(
                    source.source_path.as_str(),
                    source.source.as_str(),
                ));
        }
    }
    for source in package_sources {
        let attached = function_export_attached_members(source.source.as_str());
        if !attached.is_empty() {
            members_by_package
                .entry((source.package_name.clone(), source.package_version.clone()))
                .or_default()
                .extend(attached);
        }
    }
    members_by_package
}

pub(crate) fn promote_anonymous_cjs_wrapper_entry_thunks(
    rows: &InputRows,
    package_sources: &[PackageSource],
    report: &mut VersionedPackageMatchReport,
) {
    let already_accepted = accepted_external_modules(rows, report);
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let members_by_package = package_public_member_universe(package_sources);

    let mut promotions = Vec::<(usize, ExternalImportPromotion)>::new();
    for (idx, package_match) in report.matches.iter().enumerate() {
        if package_match.external_importable || already_accepted.contains(&package_match.module_id)
        {
            continue;
        }
        let Some(module) = modules_by_id.get(&package_match.module_id).copied() else {
            continue;
        };
        if !is_anonymous_bundle_package_candidate(rows, module) {
            continue;
        }
        let Some(slice) = rows.module_source_slice(module.id) else {
            continue;
        };
        let Some(params) = esbuild_commonjs_entry_thunk_params(slice.source) else {
            continue;
        };
        // Identity gate: the matched package's real public export names must be
        // assigned onto the thunk's exports object. This rejects a thunk that
        // merely function-matched the wrong package.
        let Some(package_members) = members_by_package.get(&(
            package_match.package_name.clone(),
            package_match.package_version.clone(),
        )) else {
            continue;
        };
        if package_members.is_empty() {
            continue;
        }
        let assigned = exports_assigned_members(slice.source, &params);
        let matched_members = package_members.intersection(&assigned).count();
        if matched_members < MIN_PUBLIC_MEMBER_ASSIGNMENTS {
            continue;
        }

        // Semantic-path proof (NOT an export-member proof): the thunk binding is
        // the package's CommonJS thunk, not one of its public members, so the
        // planner re-provides it as `function <thunk>() { return <ns>; }` (the
        // CommonJsWrapper Callable branch), never `<ns>.<thunk>`. A semantic-path
        // proof clears the planner's unproven-named-exports bail without
        // populating the member-binding map.
        let resolved_file = reverts_package::ExternalImportProofPath::semantic_path(&format!(
            "{}@{}/index.js",
            package_match.package_name, package_match.package_version,
        ));
        promotions.push((
            idx,
            ExternalImportPromotion {
                module_id: module.id,
                package_name: package_match.package_name.clone(),
                package_version: package_match.package_version.clone(),
                export_specifier: package_match.package_name.clone(),
                resolved_file,
                strategy: package_match.strategy,
                function_signature_matches: package_match.function_signature_matches,
                string_anchor_matches: package_match.string_anchor_matches,
            },
        ));
    }

    for (idx, promotion) in promotions {
        apply_external_import_promotion(report, Some(idx), promotion);
    }
}

/// The two factory parameters of an esbuild `__commonJS` wrapper:
/// `(<exports>[, <module>]) => …`. esbuild minifies both names.
pub(crate) struct ThunkParams {
    pub(crate) exports_param: String,
    pub(crate) module_param: Option<String>,
}

/// Collect the member names the thunk assigns onto its exports object, across
/// the esbuild CJS export shapes (the params are minified, so the literal
/// `exports`/`module` helpers do not apply):
///   (a) `<exports>.<member> = …`                    (e.g. react)
///   (b) `<module>.exports.<member> = …`
///   (d) `<module>.exports = { <member>: …, … }`     (object-literal aggregation,
///        e.g. semver's index.js `module.exports = {parse, valid, …}`)
/// These are the package's public surface as the bundle built it.
pub(crate) fn exports_assigned_members(source: &str, params: &ThunkParams) -> BTreeSet<String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::default()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return BTreeSet::new();
    }
    let mut visitor = ExportAssignmentVisitor {
        exports_param: params.exports_param.as_str(),
        module_param: params.module_param.as_deref(),
        members: BTreeSet::new(),
        export_function_local: None,
        members_by_object: BTreeMap::new(),
    };
    visitor.visit_program(&parsed.program);
    // (f) `<module>.exports = <fn>` then `<fn>.<member> = …` (function export with
    // attached methods, e.g. picomatch). The methods on the exported function are
    // the package's public surface.
    if let Some(local) = visitor.export_function_local.take()
        && let Some(attached) = visitor.members_by_object.remove(&local)
    {
        visitor.members.extend(attached);
    }
    visitor.members
}

struct ExportAssignmentVisitor<'s> {
    exports_param: &'s str,
    module_param: Option<&'s str>,
    members: BTreeSet<String>,
    /// Identifier assigned to `<module>.exports` (form f's exported function).
    export_function_local: Option<String>,
    /// Every `<id>.<member> =` assignment, grouped by `<id>`.
    members_by_object: BTreeMap<String, BTreeSet<String>>,
}

impl<'a> Visit<'a> for ExportAssignmentVisitor<'_> {
    fn visit_assignment_expression(&mut self, assignment: &AssignmentExpression<'a>) {
        if assignment.operator.is_assign() {
            // (a) <exports>.<member> = …
            if let Some(member) = static_member_assignment(&assignment.left, self.exports_param) {
                self.members.insert(member.to_string());
            }
            // Record every `<id>.<member> =` so form (f) can later collect the
            // exported function's attached methods.
            if let Some((object, member)) = identifier_member_assignment(&assignment.left) {
                self.members_by_object
                    .entry(object.to_string())
                    .or_default()
                    .insert(member.to_string());
            }
            if let Some(module_param) = self.module_param {
                // (b) <module>.exports.<member> = …
                if let Some(member) =
                    module_exports_member_assignment(&assignment.left, module_param)
                {
                    self.members.insert(member.to_string());
                }
                if module_exports_object_target(&assignment.left, module_param) {
                    match &assignment.right {
                        // (d) <module>.exports = { <member>: …, … }
                        Expression::ObjectExpression(object) => {
                            for member in object_expression_static_keys(object) {
                                self.members.insert(member);
                            }
                        }
                        // (f) <module>.exports = <fn>
                        Expression::Identifier(identifier) => {
                            self.export_function_local = Some(identifier.name.to_string());
                        }
                        _ => {}
                    }
                }
            }
        }
        walk_assignment_expression(self, assignment);
    }
}

/// `<object>.<member> = …` static-member assignment target → `(object, member)`.
fn identifier_member_assignment<'a>(
    target: &'a AssignmentTarget<'a>,
) -> Option<(&'a str, &'a str)> {
    let AssignmentTarget::StaticMemberExpression(member) = target else {
        return None;
    };
    let Expression::Identifier(object) = &member.object else {
        return None;
    };
    Some((object.name.as_str(), member.property.name.as_str()))
}

/// Form-(f)-ONLY public-surface extraction for a PACKAGE SOURCE: a package whose
/// entry exports a function with methods attached
/// (`module.exports = picomatch; picomatch.scan = …`). Returns the attached
/// method names. This is deliberately form-(f)-only — it never reads
/// `exports.<x>=` or object-literal forms — so an internal sub-module's
/// `exports.COMPARATOR=` (semver's `re.js`) can NEVER leak into a package's
/// public-member universe. `module` is the literal CommonJS binding here (npm
/// sources are not minified).
fn function_export_attached_members(source: &str) -> BTreeSet<String> {
    let params = ThunkParams {
        // No exports param to match (suppresses form (a)); `module` is literal.
        exports_param: String::new(),
        module_param: Some("module".to_string()),
    };
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::default()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return BTreeSet::new();
    }
    let mut visitor = ExportAssignmentVisitor {
        exports_param: params.exports_param.as_str(),
        module_param: params.module_param.as_deref(),
        members: BTreeSet::new(),
        export_function_local: None,
        members_by_object: BTreeMap::new(),
    };
    visitor.visit_program(&parsed.program);
    // Only the exported function's attached methods — discard form (a)/(d) here.
    match visitor.export_function_local {
        Some(local) => visitor.members_by_object.remove(&local).unwrap_or_default(),
        None => BTreeSet::new(),
    }
}

fn static_member_assignment<'a>(
    target: &'a AssignmentTarget<'a>,
    exports_param: &str,
) -> Option<&'a str> {
    let AssignmentTarget::StaticMemberExpression(member) = target else {
        return None;
    };
    let Expression::Identifier(object) = &member.object else {
        return None;
    };
    (object.name == exports_param).then_some(member.property.name.as_str())
}

/// `<module_param>.exports.<member>` assignment target → `<member>`.
fn module_exports_member_assignment<'a>(
    target: &'a AssignmentTarget<'a>,
    module_param: &str,
) -> Option<&'a str> {
    let AssignmentTarget::StaticMemberExpression(member) = target else {
        return None;
    };
    let Expression::StaticMemberExpression(inner) = &member.object else {
        return None;
    };
    let Expression::Identifier(object) = &inner.object else {
        return None;
    };
    (object.name == module_param && inner.property.name == "exports")
        .then_some(member.property.name.as_str())
}

/// `<module_param>.exports` assignment target (whole-object replacement).
fn module_exports_object_target(target: &AssignmentTarget<'_>, module_param: &str) -> bool {
    let AssignmentTarget::StaticMemberExpression(member) = target else {
        return false;
    };
    let Expression::Identifier(object) = &member.object else {
        return false;
    };
    object.name == module_param && member.property.name == "exports"
}

/// Recognize esbuild's minified `__commonJS` package-entry thunk and return its
/// factory parameter names. Shape:
/// `var <NS> = <helper>((<exports>[, <module>]) => { … })`.
pub(crate) fn esbuild_commonjs_entry_thunk_params(source: &str) -> Option<ThunkParams> {
    let mut search_from = 0;
    while search_from < source.len() {
        let (offset, keyword_len) = next_var_like(source, search_from)?;
        if let Some(params) =
            esbuild_commonjs_entry_thunk_params_at(&source[offset + keyword_len..])
        {
            return Some(params);
        }
        search_from = offset + keyword_len;
    }
    None
}

fn next_var_like(source: &str, search_from: usize) -> Option<(usize, usize)> {
    let var_pos = source[search_from..]
        .find("var ")
        .map(|pos| (search_from + pos, 4));
    let let_pos = source[search_from..]
        .find("let ")
        .map(|pos| (search_from + pos, 4));
    [var_pos, let_pos]
        .into_iter()
        .flatten()
        .min_by_key(|(pos, _)| *pos)
}

fn esbuild_commonjs_entry_thunk_params_at(rest: &str) -> Option<ThunkParams> {
    let eq = rest.find('=')?;
    let binding = rest[..eq].trim();
    if binding.is_empty() || !is_identifier(binding) {
        return None;
    }
    let after = rest[eq + 1..].trim_start();
    let open = after.find('(')?;
    let helper = after[..open].trim();
    if helper.is_empty() || !is_identifier(helper) {
        return None;
    }
    let inner = after[open + 1..].trim_start();
    if !inner.starts_with('(') {
        return None;
    }
    let close = inner.find(')')?;
    let arrow = inner.find("=>")?;
    if close > arrow {
        return None;
    }
    let mut parts = inner[1..close].split(',').map(str::trim);
    let exports_param = parts.next().filter(|part| is_identifier(part))?;
    let module_param = parts
        .next()
        .filter(|part| is_identifier(part))
        .map(str::to_string);
    Some(ThunkParams {
        exports_param: exports_param.to_string(),
        module_param,
    })
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}
