use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{
    ModuleDependencyTarget, PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode,
};
use reverts_ir::{
    BindingName, BindingShapeSolution, ModuleId, PackageSurface, split_bare_specifier,
};
use reverts_js::sanitize_identifier;
use reverts_model::{EnrichedProgram, PackageImportDecision, ProgramModel, SemanticNameMap};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::{PackageResolution, PackageSurfaceIndex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichmentOutput {
    pub program: EnrichedProgram,
    pub audit: AuditReport,
}

#[must_use]
pub fn enrich_program(model: ProgramModel) -> EnrichmentOutput {
    let semantic_names = assign_semantic_names(&model);
    let binding_shapes = solve_binding_shapes(&model);
    let package_index = build_package_surface_index(model.input().package_attributions.as_slice());
    let mut audit = AuditReport::default();
    let package_imports = resolve_package_imports(&model, &package_index, &mut audit);

    EnrichmentOutput {
        program: EnrichedProgram::new(model, semantic_names, package_imports, binding_shapes),
        audit,
    }
}

fn assign_semantic_names(model: &ProgramModel) -> SemanticNameMap {
    let mut semantic_names = SemanticNameMap::default();
    let mut used_by_module: BTreeMap<ModuleId, BTreeSet<String>> = BTreeMap::new();
    let mut mapped_originals = BTreeSet::<(ModuleId, String)>::new();

    for module in model.modules() {
        semantic_names.insert_module_path(module.id, module.semantic_path.clone());
    }

    for symbol in model.symbols() {
        if !mapped_originals.insert((symbol.module_id, symbol.name.clone())) {
            continue;
        }
        let base = sanitize_identifier(symbol.name.as_str());
        let semantic = reserve_unique_name(&mut used_by_module, symbol.module_id, &base);
        semantic_names.insert_binding(symbol.module_id, symbol.name.clone(), semantic);
    }

    semantic_names
}

fn reserve_unique_name(
    used_by_module: &mut BTreeMap<ModuleId, BTreeSet<String>>,
    module_id: ModuleId,
    base: &str,
) -> String {
    let used = used_by_module.entry(module_id).or_default();
    if used.insert(base.to_string()) {
        return base.to_string();
    }

    let mut suffix = 2_u32;
    loop {
        let candidate = format!("{base}_{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

fn solve_binding_shapes(model: &ProgramModel) -> BindingShapeSolution {
    let mut solution = BindingShapeSolution::default();
    for constraint in model.graph().def_use().constraints() {
        solution.add_constraint(constraint);
    }
    solution
}

fn build_package_surface_index(attributions: &[PackageAttributionInput]) -> PackageSurfaceIndex {
    let mut surfaces = BTreeMap::<String, PackageSurface>::new();

    for attribution in attributions {
        if attribution.status != PackageAttributionStatus::Accepted
            || attribution.emission_mode != PackageEmissionMode::ExternalImport
        {
            continue;
        }

        if let Some(specifier) = attribution.export_specifier.as_deref() {
            insert_surface_specifier(&mut surfaces, attribution.package_name.as_str(), specifier);
        }

        if let Some(subpath) = attribution.subpath.as_deref() {
            insert_surface_subpath(&mut surfaces, attribution.package_name.as_str(), subpath);
        }
    }

    let mut index = PackageSurfaceIndex::default();
    for surface in surfaces.into_values() {
        index.insert(surface);
    }
    index
}

fn insert_surface_specifier(
    surfaces: &mut BTreeMap<String, PackageSurface>,
    package_name: &str,
    specifier: &str,
) {
    let Some((resolved_package, subpath)) = split_bare_specifier(specifier) else {
        return;
    };
    if resolved_package != package_name {
        return;
    }

    match subpath {
        Some(subpath) => insert_surface_subpath(surfaces, package_name, subpath.as_str()),
        None => {
            let surface = surfaces
                .remove(package_name)
                .unwrap_or_else(|| PackageSurface::new(package_name))
                .with_root_importable();
            surfaces.insert(package_name.to_string(), surface);
        }
    }
}

fn insert_surface_subpath(
    surfaces: &mut BTreeMap<String, PackageSurface>,
    package_name: &str,
    subpath: &str,
) {
    let surface = surfaces
        .remove(package_name)
        .unwrap_or_else(|| PackageSurface::new(package_name))
        .with_subpath(subpath);
    surfaces.insert(package_name.to_string(), surface);
}

fn resolve_package_imports(
    model: &ProgramModel,
    package_index: &PackageSurfaceIndex,
    audit: &mut AuditReport,
) -> Vec<PackageImportDecision> {
    let mut decisions = Vec::new();

    for dependency in &model.input().dependencies {
        let ModuleDependencyTarget::Package { specifier } = &dependency.target else {
            continue;
        };

        let resolution = package_index.resolve(specifier);
        if let PackageResolution::Rejected { reason, .. } = &resolution {
            audit.push(
                AuditFinding::error(FindingCode::UnresolvableBareImport, reason.clone())
                    .with_module(dependency.from_module_id.0.to_string())
                    .with_binding(specifier.clone()),
            );
        }

        decisions.push(PackageImportDecision::new(
            dependency.from_module_id,
            BindingName::new(package_namespace_binding(specifier)),
            resolution,
        ));
    }

    decisions
}

fn package_namespace_binding(specifier: &str) -> String {
    let sanitized = sanitize_identifier(specifier);
    format!("__pkg_{sanitized}")
}

#[cfg(test)]
mod tests {
    use reverts_input::{
        InputBundle, InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
        PackageAttributionInput, ProjectInput, SymbolInput,
    };
    use reverts_ir::ModuleId;
    use reverts_observe::FindingCode;
    use reverts_package::PackageResolution;

    use super::{ProgramModel, enrich_program};

    fn valid_rows() -> InputRows {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "app", "src/index.ts"));
        rows
    }

    #[test]
    fn accepted_attribution_resolves_package_dependency() {
        let mut rows = valid_rows();
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "lodash/map".to_string(),
            },
        });
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.is_clean());
        assert!(matches!(
            output.program.package_imports()[0].resolution,
            PackageResolution::External { .. }
        ));
    }

    #[test]
    fn semantic_naming_sanitizes_reserved_words() {
        let mut rows = valid_rows();
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "class".to_string(),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));
        let binding = output
            .program
            .semantic_names()
            .binding_name(ModuleId(1), "class")
            .expect("semantic binding should exist");

        assert_eq!(binding.as_str(), "_class");
    }

    #[test]
    fn unknown_package_surface_reports_unresolvable_import() {
        let mut rows = valid_rows();
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "lodash/map".to_string(),
            },
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::UnresolvableBareImport));
        assert!(matches!(
            output.program.package_imports()[0].resolution,
            PackageResolution::Rejected { .. }
        ));
    }
}
