//! Source-backed bare-import / require / dynamic-import site discovery.
//! Walks original source files (not bundle modules) with OXC and recovers
//! the set of bare package specifiers that appear in code, including the
//! resolved-surface attributions derived from that walk.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    Visit,
    ast::{
        Argument, CallExpression, ExportAllDeclaration, ExportNamedDeclaration, Expression,
        ImportDeclaration, ImportExpression,
    },
    visit::walk::{
        walk_call_expression, walk_export_all_declaration, walk_export_named_declaration,
        walk_import_expression,
    },
};
use oxc_parser::Parser;
use reverts_input::{
    InputRows, PackageAttributionInput, PackageAttributionStatus, PackageSurfaceInput,
};
use reverts_ir::{ModuleKind, is_valid_package_name, split_bare_specifier};
use reverts_js::{JsError, ParseError, ParseGoal, parse_options_for, source_type_candidates};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::{is_accepted_external_attribution, is_node_builtin};

use crate::{PackageImportSite, PackageSource, SourcePackageImportParseError};

pub fn package_import_names_from_sources(
    rows: &InputRows,
) -> Result<BTreeSet<String>, SourcePackageImportParseError> {
    Ok(package_import_sites_from_sources(rows)?
        .into_iter()
        .map(|site| site.package_name)
        .collect())
}

/// Extracts source-backed bare package import/require sites from whole source
/// files rather than from package-module rows. This is the path used for
/// packages such as `ws`/`undici` that appear as runtime dependencies but whose
/// implementation is not bundled as a module.
pub fn package_import_sites_from_sources(
    rows: &InputRows,
) -> Result<BTreeSet<PackageImportSite>, SourcePackageImportParseError> {
    let mut sites = BTreeSet::new();
    for source_file in &rows.source_files {
        let Some(source) = source_file.source.as_deref() else {
            continue;
        };
        sites.extend(package_import_sites_from_source_file(
            source_file.id,
            source_file.path.as_str(),
            source,
        )?);
    }
    Ok(sites)
}

pub(crate) fn resolve_source_package_surfaces(
    rows: &InputRows,
    current_attributions: &[PackageAttributionInput],
    package_sources: &[PackageSource],
    package_filter: Option<&BTreeSet<String>>,
    audit: &mut AuditReport,
) -> Vec<PackageSurfaceInput> {
    let mut sites_by_specifier = BTreeMap::<(String, String), BTreeSet<String>>::new();
    let import_sites = match package_import_sites_from_sources(rows) {
        Ok(sites) => sites,
        Err(error) => {
            audit.push(
                AuditFinding::error(
                    FindingCode::AstFactExtractionFailed,
                    format!(
                        "failed to parse source-backed package import sites: {}",
                        error.source
                    ),
                )
                .with_module(error.source_file_path),
            );
            return Vec::new();
        }
    };
    for site in import_sites {
        if let Some(package_filter) = package_filter
            && !package_filter.contains(site.package_name.as_str())
        {
            continue;
        }
        if has_accepted_surface(rows, site.specifier.as_str()) {
            continue;
        }
        sites_by_specifier
            .entry((site.package_name, site.specifier))
            .or_default()
            .insert(site.source_file_path);
    }

    let mut surfaces = Vec::new();
    for ((package_name, specifier), source_paths) in sites_by_specifier {
        let Some((package_version, evidence_kind)) = external_package_version(
            rows,
            current_attributions,
            package_sources,
            package_name.as_str(),
        ) else {
            audit.push(
                AuditFinding::error(
                    FindingCode::AmbiguousPackageSurfaceVersion,
                    "source-backed package import has no unique package version; external import surface was not accepted",
                )
                .with_binding(specifier.clone()),
            );
            continue;
        };
        let evidence = source_surface_evidence(
            package_name.as_str(),
            package_version.as_str(),
            specifier.as_str(),
            evidence_kind,
            &source_paths,
        );
        surfaces.push(
            PackageSurfaceInput::accepted_external(package_name, package_version, specifier)
                .with_evidence(evidence),
        );
    }
    surfaces
}

fn has_accepted_surface(rows: &InputRows, specifier: &str) -> bool {
    rows.package_surfaces.iter().any(|surface| {
        surface.status == PackageAttributionStatus::Accepted
            && surface.export_specifier.as_str() == specifier
    })
}

fn package_import_sites_from_source_file(
    source_file_id: u32,
    source_file_path: &str,
    source: &str,
) -> Result<BTreeSet<PackageImportSite>, SourcePackageImportParseError> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();
    for source_type in
        source_type_candidates(Some(Path::new(source_file_path)), ParseGoal::TypeScript)
    {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.errors.is_empty() && !parsed.panicked {
            let mut visitor = SourcePackageImportVisitor::default();
            visitor.visit_program(&parsed.program);
            return Ok(visitor
                .specifiers
                .into_iter()
                .map(|(package_name, specifier)| PackageImportSite {
                    source_file_id,
                    source_file_path: source_file_path.to_string(),
                    package_name,
                    specifier,
                })
                .collect());
        }
        errors.push(ParseError {
            source_type: format!("{source_type:?}"),
            diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
        });
    }
    Err(SourcePackageImportParseError {
        source_file_id,
        source_file_path: source_file_path.to_string(),
        source: JsError::ParseFailed(errors),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceVersionEvidenceKind {
    PackageModule,
    AcceptedAttribution,
    CachedPackageSource,
}

impl SurfaceVersionEvidenceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PackageModule => "package_module_version",
            Self::AcceptedAttribution => "accepted_attribution_version",
            Self::CachedPackageSource => "cached_package_source_version",
        }
    }
}

fn external_package_version(
    rows: &InputRows,
    current_attributions: &[PackageAttributionInput],
    package_sources: &[PackageSource],
    package_name: &str,
) -> Option<(String, SurfaceVersionEvidenceKind)> {
    let module_versions = rows
        .modules
        .iter()
        .filter(|module| {
            module.kind == ModuleKind::Package
                && module.package_name.as_deref() == Some(package_name)
        })
        .filter_map(|module| module.package_version.clone())
        .collect::<BTreeSet<_>>();
    if let Some(version) = unique_version(module_versions) {
        return Some((version, SurfaceVersionEvidenceKind::PackageModule));
    }

    let attribution_versions = rows
        .package_attributions
        .iter()
        .chain(current_attributions.iter())
        .filter(|attribution| {
            attribution.package_name == package_name
                && is_accepted_external_attribution(attribution)
        })
        .filter_map(|attribution| attribution.package_version.clone())
        .collect::<BTreeSet<_>>();
    if let Some(version) = unique_version(attribution_versions) {
        return Some((version, SurfaceVersionEvidenceKind::AcceptedAttribution));
    }

    let cached_versions = package_sources
        .iter()
        .filter(|source| source.package_name == package_name)
        .map(|source| source.package_version.clone())
        .collect::<BTreeSet<_>>();
    if let Some(version) = unique_version(cached_versions) {
        return Some((version, SurfaceVersionEvidenceKind::CachedPackageSource));
    }

    None
}

fn unique_version(versions: BTreeSet<String>) -> Option<String> {
    if versions.len() == 1 {
        versions.into_iter().next()
    } else {
        None
    }
}

fn source_surface_evidence(
    package_name: &str,
    package_version: &str,
    export_specifier: &str,
    evidence_kind: SurfaceVersionEvidenceKind,
    source_paths: &BTreeSet<String>,
) -> String {
    serde_json::json!({
        "matcher": "source_package_import_surface",
        "package_name": package_name,
        "package_version": package_version,
        "export_specifier": export_specifier,
        "version_evidence": evidence_kind.as_str(),
        "source_paths": source_paths.iter().collect::<Vec<_>>(),
    })
    .to_string()
}

#[derive(Debug, Default)]
struct SourcePackageImportVisitor {
    specifiers: BTreeSet<(String, String)>,
}

impl<'a> Visit<'a> for SourcePackageImportVisitor {
    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        self.record_specifier(it.source.value.as_str());
    }

    fn visit_export_named_declaration(&mut self, it: &ExportNamedDeclaration<'a>) {
        if let Some(source) = &it.source {
            self.record_specifier(source.value.as_str());
        }
        walk_export_named_declaration(self, it);
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        self.record_specifier(it.source.value.as_str());
        walk_export_all_declaration(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if let Expression::Identifier(identifier) = &it.callee
            && identifier.name.as_str() == "require"
            && let Some(specifier) = it.arguments.first().and_then(argument_string_literal)
        {
            self.record_specifier(specifier);
        }
        walk_call_expression(self, it);
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Some(specifier) = expression_string_literal(&it.source) {
            self.record_specifier(specifier);
        }
        walk_import_expression(self, it);
    }
}

impl SourcePackageImportVisitor {
    fn record_specifier(&mut self, specifier: &str) {
        if is_node_builtin(specifier) {
            return;
        }
        let Some((package_name, _subpath)) = split_bare_specifier(specifier) else {
            return;
        };
        if !is_valid_package_name(package_name.as_str()) {
            return;
        }
        self.specifiers
            .insert((package_name, specifier.to_string()));
    }
}

fn argument_string_literal<'a>(argument: &'a Argument<'a>) -> Option<&'a str> {
    match argument {
        Argument::StringLiteral(literal) => Some(literal.value.as_str()),
        _ => None,
    }
}

fn expression_string_literal<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::StringLiteral(literal) => Some(literal.value.as_str()),
        _ => None,
    }
}
