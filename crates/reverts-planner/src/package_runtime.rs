//! Package-runtime "island" planning and helper emission.
//!
//! Many bundled programs include multiple internal modules that all
//! belong to the same npm package and share private helper closures.
//! Without consolidation, every emitted module that needs one of those
//! private helpers would re-emit a copy of the closure and import any
//! shared bindings independently. `package_runtime_island_plan`
//! decides where to put those shared helpers — typically a single
//! `modules/package-runtime/<scope>/<package>/<version>/source-<id>-helpers.ts`
//! file per (package, source-file-id) tuple — and routes consumer
//! modules to import from there.
//!
//! The pass has two phases:
//!
//! 1. **Planning.** `package_runtime_island_plan` walks the runtime
//!    prelude, identifies closures private to a single package, gates
//!    them through `package_runtime_closure_is_safe` /
//!    `package_runtime_helper_imports_are_safe`, and records each
//!    binding's owner in `PackageRuntimeIslandPlan.owners_by_binding`.
//! 2. **Emission.** During the per-module loop, `partition_package_runtime_bindings`
//!    splits the runtime helpers each consumer needs into "import from
//!    the package-runtime helper file" vs. "still needs the shared
//!    runtime helpers file". `PackageRuntimeImportEmitter` writes the
//!    consumer-side imports; `emit_package_runtime_helper_files`
//!    writes the helper file itself once per (key, source-file-id);
//!    `inline_package_runtime_helper_into_single_consumer` is the
//!    single-consumer fallback that skips the helper file entirely.
//!
//! `push_packed_runtime_helper_imports` and
//! `packed_named_import_statements` are the small import-coalescing
//! helpers that emit canonical packed `import { … } from '…';`
//! statements for the package-runtime helper imports.

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::RuntimePrelude;
use reverts_input::ModuleInput;
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
use reverts_model::EnrichedProgram;

use crate::binding_owner::BindingOwnerPlan;
use crate::identifiers::is_planner_synthetic_binding;
use crate::import_coalesce::coalesce_top_level_import_declarations;
use crate::relative_paths::relative_import_specifier;
use crate::runtime_helper_writes::{
    inline_internal_setter_calls, inline_internal_setter_calls_for_bindings,
};
use crate::runtime_source_read::{
    RuntimeSourceReadIndex, runtime_readers_for_binding, runtime_source_read_index,
};
use crate::statement_parsers::parse_generated_named_import_statement;
use crate::statements::{
    lazy_module_helper_source, lazy_value_helper_source, named_export_statement,
    named_import_statement, noop_function_statement, runtime_helper_import_statement,
    runtime_helper_setter_declarations, runtime_helper_setter_name,
};
use crate::{
    ClosedRuntimeHelperSource, EmitPlan, LoweredRuntimeModuleSource, PlanError, PlannedBinding,
    PlannedFile, RuntimeLazyFoldPlan, RuntimeVarMigrationPlan, close_runtime_helper_source,
    coalesce_runtime_lazy_initializer_call_runs, compact_pure_static_runtime_literals,
    ensure_planned_module_exports, module_dependency_modules_by_owner, module_output_path,
    plan_binding_from_program, prune_orphan_runtime_bindings,
    purify_private_runtime_lazy_initializers, runtime_binding_has_blocking_non_snippet_use,
    scan_runtime_externalized_bindings, unresolved_runtime_helper_references,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PackageRuntimeOwner {
    pub(crate) name: String,
    pub(crate) version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PackageRuntimeHelperKey {
    pub(crate) owner: PackageRuntimeOwner,
    pub(crate) source_file_id: u32,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PackageRuntimeHelperUsage {
    pub(crate) public_bindings: BTreeSet<BindingName>,
    pub(crate) required_bindings: BTreeSet<BindingName>,
    pub(crate) setter_bindings: BTreeSet<BindingName>,
    pub(crate) consumer_modules: BTreeSet<ModuleId>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PackageRuntimeIslandPlan {
    pub(crate) owners_by_binding: BTreeMap<(u32, BindingName), PackageRuntimeOwner>,
}

pub(crate) fn package_runtime_owner_for_module(
    module: &ModuleInput,
    externalized_packages: &BTreeSet<ModuleId>,
) -> Option<PackageRuntimeOwner> {
    if module.kind != ModuleKind::Package || externalized_packages.contains(&module.id) {
        return None;
    }
    Some(PackageRuntimeOwner {
        name: module.package_name.clone()?,
        version: module.package_version.clone()?,
    })
}

pub(crate) fn package_runtime_helpers_path(
    owner: &PackageRuntimeOwner,
    source_file_id: u32,
) -> String {
    let package = sanitize_package_runtime_path_segment(owner.name.as_str());
    let version = sanitize_package_runtime_path_segment(owner.version.as_str());
    format!("modules/package-runtime/{package}-{version}/source-{source_file_id}-helpers.ts")
}

pub(crate) fn sanitize_package_runtime_path_segment(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

pub(crate) fn package_runtime_island_plan(
    program: &EnrichedProgram,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    runtime_var_migrations: &RuntimeVarMigrationPlan,
    externalized_packages: &BTreeSet<ModuleId>,
) -> PackageRuntimeIslandPlan {
    let module_owners = program
        .model()
        .modules()
        .iter()
        .map(|module| {
            (
                module.id,
                package_runtime_owner_for_module(module, externalized_packages),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let mut owners_by_binding = BTreeMap::<(u32, BindingName), PackageRuntimeOwner>::new();
    let mut consumers_by_binding = BTreeMap::<(u32, BindingName), BTreeSet<ModuleId>>::new();
    let mut blocked_bindings = BTreeSet::<(u32, BindingName)>::new();

    for module in program.model().modules() {
        if module.kind == ModuleKind::Package && externalized_packages.contains(&module.id) {
            continue;
        }
        let owner = module_owners.get(&module.id).and_then(Option::as_ref);
        let mut used_by_source = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        for import in program.model().graph().runtime_imports_for(module.id) {
            used_by_source
                .entry(import.source_file_id)
                .or_default()
                .insert(import.binding);
        }
        if let Some(source) = lowered_runtime_sources.get(&module.id) {
            used_by_source
                .entry(source.source_file_id)
                .or_default()
                .extend(source.remaining_helpers.iter().cloned());
            used_by_source
                .entry(source.source_file_id)
                .or_default()
                .extend(source.written_helpers.iter().cloned());
        }

        for (source_file_id, bindings) in used_by_source {
            for binding in bindings {
                let key = (source_file_id, binding.clone());
                if is_package_runtime_excluded_binding(&binding)
                    || runtime_var_migrations
                        .migrated_owner(source_file_id, &binding)
                        .is_some()
                {
                    blocked_bindings.insert(key);
                    continue;
                }
                let Some(owner) = owner else {
                    blocked_bindings.insert(key);
                    continue;
                };
                consumers_by_binding
                    .entry(key.clone())
                    .or_default()
                    .insert(module.id);
                match owners_by_binding.get(&key) {
                    Some(existing_owner) if existing_owner != owner => {
                        blocked_bindings.insert(key);
                    }
                    Some(_) => {}
                    None => {
                        owners_by_binding.insert(key, owner.clone());
                    }
                }
            }
        }
    }

    for key in &blocked_bindings {
        owners_by_binding.remove(key);
    }

    let mut roots_by_key = BTreeMap::<PackageRuntimeHelperKey, BTreeSet<BindingName>>::new();
    for ((source_file_id, binding), owner) in &owners_by_binding {
        if blocked_bindings.contains(&(*source_file_id, binding.clone())) {
            continue;
        }
        if let Some(prelude) = program.model().graph().runtime_prelude(*source_file_id)
            && prelude.defines(binding)
        {
            roots_by_key
                .entry(PackageRuntimeHelperKey {
                    owner: owner.clone(),
                    source_file_id: *source_file_id,
                })
                .or_default()
                .insert(binding.clone());
        }
    }

    let mut plan = PackageRuntimeIslandPlan::default();
    for (key, mut root_bindings) in roots_by_key {
        let Some(prelude) = program.model().graph().runtime_prelude(key.source_file_id) else {
            continue;
        };
        root_bindings.retain(|binding| {
            !is_package_runtime_excluded_binding(binding)
                && runtime_var_migrations
                    .migrated_owner(key.source_file_id, binding)
                    .is_none()
                && prelude.defines(binding)
        });
        if root_bindings.is_empty() {
            continue;
        }
        let folded_chunks = runtime_lazy_folds
            .chunks_by_source_file
            .get(&key.source_file_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let read_index = runtime_source_read_index(prelude, folded_chunks);
        let mut helper_closure = close_runtime_helper_source(prelude, &root_bindings, None, &[]);
        helper_closure.source = inline_internal_setter_calls(&helper_closure.source);
        helper_closure.source = purify_private_runtime_lazy_initializers(
            helper_closure.source.as_str(),
            &helper_closure.emitted_bindings,
        );
        let gate = PackageRuntimeClosureGate {
            prelude,
            read_index: &read_index,
            source_file_id: key.source_file_id,
            owner: &key.owner,
            owners_by_binding: &owners_by_binding,
            blocked_bindings: &blocked_bindings,
            runtime_var_migrations,
        };
        if helper_closure.emitted_bindings.is_empty()
            || !package_runtime_closure_is_safe(&gate, &helper_closure)
        {
            continue;
        }
        let runtime_externalized_binding_scan = scan_runtime_externalized_bindings(
            program,
            helper_closure.source.as_str(),
            &helper_closure.emitted_bindings,
            externalized_packages,
        );
        let helper_imports = runtime_externalized_binding_scan.source_module_imports;
        let consumers = root_bindings
            .iter()
            .flat_map(|binding| {
                consumers_by_binding
                    .get(&(key.source_file_id, binding.clone()))
                    .into_iter()
                    .flatten()
                    .copied()
            })
            .collect::<BTreeSet<_>>();
        if !package_runtime_helper_imports_are_safe(
            program,
            externalized_packages,
            &modules_by_id,
            &key.owner,
            &consumers,
            &helper_imports,
        ) {
            continue;
        }
        let unresolved = unresolved_runtime_helper_references(
            prelude,
            helper_closure.source.as_str(),
            &helper_closure.emitted_bindings,
            &helper_imports,
        );
        if !unresolved.is_empty() {
            continue;
        }
        for binding in &helper_closure.emitted_bindings {
            plan.owners_by_binding
                .insert((key.source_file_id, binding.clone()), key.owner.clone());
        }
    }

    plan
}

pub(crate) fn is_package_runtime_excluded_binding(binding: &BindingName) -> bool {
    let name = binding.as_str();
    name == "lazyModule"
        || name == "lazyValue"
        || name.starts_with("__reverts_set_")
        || is_planner_synthetic_binding(name)
}

pub(crate) struct PackageRuntimeClosureGate<'a> {
    pub(crate) prelude: &'a RuntimePrelude,
    pub(crate) read_index: &'a RuntimeSourceReadIndex,
    pub(crate) source_file_id: u32,
    pub(crate) owner: &'a PackageRuntimeOwner,
    pub(crate) owners_by_binding: &'a BTreeMap<(u32, BindingName), PackageRuntimeOwner>,
    pub(crate) blocked_bindings: &'a BTreeSet<(u32, BindingName)>,
    pub(crate) runtime_var_migrations: &'a RuntimeVarMigrationPlan,
}

pub(crate) fn package_runtime_closure_is_safe(
    gate: &PackageRuntimeClosureGate<'_>,
    helper_closure: &ClosedRuntimeHelperSource,
) -> bool {
    for binding in &helper_closure.emitted_bindings {
        if !gate.prelude.defines(binding)
            || is_package_runtime_excluded_binding(binding)
            || gate
                .runtime_var_migrations
                .migrated_owner(gate.source_file_id, binding)
                .is_some()
            || runtime_binding_has_blocking_non_snippet_use(gate.read_index, binding)
            || gate
                .blocked_bindings
                .contains(&(gate.source_file_id, binding.clone()))
        {
            return false;
        }
        if let Some(existing_owner) = gate
            .owners_by_binding
            .get(&(gate.source_file_id, binding.clone()))
            && existing_owner != gate.owner
        {
            return false;
        }
        if runtime_readers_for_binding(gate.read_index, binding)
            .into_iter()
            .any(|reader| !helper_closure.emitted_bindings.contains(&reader))
        {
            return false;
        }
    }
    true
}

pub(crate) fn package_runtime_helper_imports_are_safe(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    owner: &PackageRuntimeOwner,
    consumers: &BTreeSet<ModuleId>,
    helper_imports: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) -> bool {
    for module_id in helper_imports.keys() {
        let Some(module) = modules_by_id.get(module_id).copied() else {
            return false;
        };
        if package_runtime_owner_for_module(module, externalized_packages).as_ref() != Some(owner) {
            return false;
        }
        if consumers.contains(module_id)
            || module_dependency_reaches_any(program, *module_id, consumers)
        {
            return false;
        }
    }
    true
}

pub(crate) fn module_dependency_reaches_any(
    program: &EnrichedProgram,
    start: ModuleId,
    targets: &BTreeSet<ModuleId>,
) -> bool {
    if targets.is_empty() {
        return false;
    }
    let dependencies = module_dependency_modules_by_owner(program);
    let mut seen = BTreeSet::<ModuleId>::new();
    let mut stack = dependencies
        .get(&start)
        .into_iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    while let Some(module_id) = stack.pop() {
        if targets.contains(&module_id) {
            return true;
        }
        if !seen.insert(module_id) {
            continue;
        }
        if let Some(next) = dependencies.get(&module_id) {
            stack.extend(next.iter().copied());
        }
    }
    false
}

pub(crate) fn partition_package_runtime_bindings(
    binding_owners: &BindingOwnerPlan,
    package_runtime_owner: Option<&PackageRuntimeOwner>,
    source_file_id: u32,
    bindings: &BTreeSet<BindingName>,
) -> (BTreeSet<BindingName>, BTreeSet<BindingName>) {
    let mut package_bindings = BTreeSet::<BindingName>::new();
    let mut runtime_bindings = BTreeSet::<BindingName>::new();
    for binding in bindings {
        if let Some(owner) = package_runtime_owner
            && binding_owners
                .package_runtime_owner(source_file_id, binding)
                .is_some_and(|candidate| candidate == owner)
        {
            package_bindings.insert(binding.clone());
            continue;
        }
        runtime_bindings.insert(binding.clone());
    }
    (package_bindings, runtime_bindings)
}

pub(crate) struct PackageRuntimeImportEmitter<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) used_package_runtime_helper_files:
        &'a mut BTreeMap<PackageRuntimeHelperKey, PackageRuntimeHelperUsage>,
    pub(crate) file: &'a mut PlannedFile,
    pub(crate) planned_bindings: &'a mut BTreeSet<BindingName>,
    pub(crate) module_id: ModuleId,
    pub(crate) module_path: &'a str,
    pub(crate) owner: &'a PackageRuntimeOwner,
    pub(crate) source_file_id: u32,
}

pub(crate) fn emit_package_runtime_helper_import(
    emitter: &mut PackageRuntimeImportEmitter<'_>,
    bindings: &BTreeSet<BindingName>,
    setter_bindings: &BTreeSet<BindingName>,
) {
    let key = PackageRuntimeHelperKey {
        owner: emitter.owner.clone(),
        source_file_id: emitter.source_file_id,
    };
    let usage = emitter
        .used_package_runtime_helper_files
        .entry(key.clone())
        .or_default();
    usage.consumer_modules.insert(emitter.module_id);
    usage.public_bindings.extend(bindings.iter().cloned());
    usage.required_bindings.extend(bindings.iter().cloned());
    usage
        .required_bindings
        .extend(setter_bindings.iter().cloned());
    usage
        .setter_bindings
        .extend(setter_bindings.iter().cloned());

    let helper_path = package_runtime_helpers_path(emitter.owner, emitter.source_file_id);
    let specifier = relative_import_specifier(emitter.module_path, helper_path.as_str());
    emitter.file.push_source(runtime_helper_import_statement(
        bindings,
        setter_bindings,
        &[],
        specifier.as_str(),
    ));
    for binding in bindings {
        if emitter.planned_bindings.contains(binding) {
            continue;
        }
        emitter.planned_bindings.insert(binding.clone());
        emitter.file.add_binding(plan_binding_from_program(
            emitter.program,
            emitter.module_id,
            binding.clone(),
            binding.clone(),
            true,
            None,
        ));
    }
}

pub(crate) fn inline_package_runtime_helper_into_single_consumer(
    program: &EnrichedProgram,
    plan: &mut EmitPlan,
    usage: &PackageRuntimeHelperUsage,
    helper_path: &str,
    helper_closure: &ClosedRuntimeHelperSource,
    helper_imports: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
    package_init_shims: &BTreeSet<BindingName>,
) -> Result<bool, PlanError> {
    let Some(consumer_module) = usage.consumer_modules.iter().next().copied() else {
        return Ok(false);
    };
    if usage.consumer_modules.len() != 1 {
        return Ok(false);
    }
    if helper_closure.source.contains("lazyModule(") || helper_closure.source.contains("lazyValue(")
    {
        return Ok(false);
    }
    let Some(consumer_path) = module_output_path(program, consumer_module) else {
        return Ok(false);
    };
    let helper_specifier = relative_import_specifier(consumer_path.as_str(), helper_path);

    for (module_id, bindings) in helper_imports {
        ensure_planned_module_exports(plan, program, *module_id, bindings);
    }

    let mut replacement = Vec::<String>::new();
    for (module_id, bindings) in helper_imports {
        let Some(module_path) = module_output_path(program, *module_id) else {
            continue;
        };
        let specifier = relative_import_specifier(consumer_path.as_str(), module_path.as_str());
        replacement.push(named_import_statement(bindings.iter(), specifier.as_str()));
    }
    for binding in package_init_shims {
        replacement.push(noop_function_statement(binding));
    }
    if !helper_closure.source.trim().is_empty() {
        replacement.push(helper_closure.source.clone());
    }

    let Some(file) = plan
        .files
        .iter_mut()
        .find(|file| file.path == consumer_path)
    else {
        return Ok(false);
    };
    let mut inserted = false;
    let mut body = Vec::<String>::new();
    for source in std::mem::take(&mut file.body) {
        if parse_generated_named_import_statement(source.as_str())
            .is_some_and(|(_bindings, specifier)| specifier == helper_specifier)
        {
            if !inserted {
                body.extend(replacement.iter().cloned());
                inserted = true;
            }
            continue;
        }
        body.push(source);
    }
    if !inserted {
        file.body = body;
        return Ok(false);
    }

    if !usage.setter_bindings.is_empty() {
        for source in &mut body {
            *source =
                inline_internal_setter_calls_for_bindings(source.as_str(), &usage.setter_bindings);
        }
    }
    file.body = body;
    crate::import_coalesce::coalesce_generated_named_imports(file);

    Ok(true)
}

pub(crate) fn emit_package_runtime_helper_files(
    program: &EnrichedProgram,
    plan: &mut EmitPlan,
    used_package_runtime_helper_files: &BTreeMap<
        PackageRuntimeHelperKey,
        PackageRuntimeHelperUsage,
    >,
    externalized_packages: &BTreeSet<ModuleId>,
) -> Result<(), PlanError> {
    for (key, usage) in used_package_runtime_helper_files {
        let Some(prelude) = program.model().graph().runtime_prelude(key.source_file_id) else {
            continue;
        };
        let mut root_bindings = usage.required_bindings.clone();
        root_bindings.extend(usage.setter_bindings.iter().cloned());
        if root_bindings.is_empty() {
            continue;
        }
        let mut helper_closure = close_runtime_helper_source(prelude, &root_bindings, None, &[]);
        helper_closure.source = inline_internal_setter_calls(&helper_closure.source);
        helper_closure.source = purify_private_runtime_lazy_initializers(
            helper_closure.source.as_str(),
            &helper_closure.emitted_bindings,
        );
        helper_closure.source =
            coalesce_runtime_lazy_initializer_call_runs(helper_closure.source.as_str());
        helper_closure.source =
            compact_pure_static_runtime_literals(helper_closure.source.as_str());
        helper_closure.source =
            coalesce_top_level_import_declarations(helper_closure.source.as_str());
        let mut runtime_binding_roots = usage.public_bindings.clone();
        runtime_binding_roots.extend(usage.setter_bindings.iter().cloned());
        let orphan_prune =
            prune_orphan_runtime_bindings(helper_closure.source.as_str(), &runtime_binding_roots);
        helper_closure.source = orphan_prune.source;
        for binding in &orphan_prune.dropped_bindings {
            helper_closure.emitted_bindings.remove(binding);
        }
        let helper_path = package_runtime_helpers_path(&key.owner, key.source_file_id);
        let runtime_externalized_binding_scan = scan_runtime_externalized_bindings(
            program,
            helper_closure.source.as_str(),
            &helper_closure.emitted_bindings,
            externalized_packages,
        );
        let helper_imports = runtime_externalized_binding_scan.source_module_imports;
        let package_init_shims = runtime_externalized_binding_scan.package_init_shims;
        let mut emitted_runtime_bindings = helper_closure.emitted_bindings.clone();
        emitted_runtime_bindings.extend(package_init_shims.iter().cloned());
        let unresolved = unresolved_runtime_helper_references(
            prelude,
            helper_closure.source.as_str(),
            &emitted_runtime_bindings,
            &helper_imports,
        );
        if !unresolved.is_empty() {
            return Err(PlanError::UnresolvedRuntimeHelperReferences {
                path: helper_path,
                bindings: unresolved.into_iter().collect(),
            });
        }

        if inline_package_runtime_helper_into_single_consumer(
            program,
            plan,
            usage,
            helper_path.as_str(),
            &helper_closure,
            &helper_imports,
            &package_init_shims,
        )? {
            continue;
        }

        let mut file = PlannedFile::new(helper_path.clone());
        push_packed_runtime_helper_imports(
            program,
            plan,
            &mut file,
            helper_path.as_str(),
            &helper_imports,
        );
        for binding in &package_init_shims {
            file.push_source(noop_function_statement(binding));
        }
        let emits_lazy_module = helper_closure.source.contains("lazyModule(");
        let emits_lazy_value = helper_closure.source.contains("lazyValue(");
        if !helper_closure.source.trim().is_empty() {
            file.push_source(helper_closure.source);
        }
        if !usage.setter_bindings.is_empty() {
            file.push_source(runtime_helper_setter_declarations(&usage.setter_bindings));
        }
        if emits_lazy_module {
            file.push_source(lazy_module_helper_source());
        }
        if emits_lazy_value {
            file.push_source(lazy_value_helper_source());
        }

        let mut exported_bindings = usage.public_bindings.clone();
        exported_bindings.extend(
            usage
                .setter_bindings
                .iter()
                .map(|binding| BindingName::new(runtime_helper_setter_name(binding))),
        );
        if !exported_bindings.is_empty() {
            file.push_source(named_export_statement(exported_bindings.iter()));
        }
        for binding in &usage.public_bindings {
            file.add_binding(PlannedBinding::new(
                binding.clone(),
                binding.clone(),
                BindingShape::Unknown,
                true,
            ));
            file.add_export_with_source_backed(binding.clone(), true);
        }
        for setter in usage
            .setter_bindings
            .iter()
            .map(|binding| BindingName::new(runtime_helper_setter_name(binding)))
        {
            file.add_binding(PlannedBinding::new(
                setter.clone(),
                setter.clone(),
                BindingShape::Callable,
                true,
            ));
            file.add_export_with_source_backed(setter, true);
        }
        if file.body.is_empty() {
            continue;
        }
        crate::finalize_planned_file(&mut file);
        plan.push_file(file);
    }
    Ok(())
}

pub(crate) fn push_packed_runtime_helper_imports(
    program: &EnrichedProgram,
    plan: &mut EmitPlan,
    file: &mut PlannedFile,
    helper_path: &str,
    helper_imports: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) {
    let mut imports = Vec::<(String, BTreeSet<BindingName>)>::new();
    for (module_id, bindings) in helper_imports {
        ensure_planned_module_exports(plan, program, *module_id, bindings);
        let Some(module_path) = module_output_path(program, *module_id) else {
            continue;
        };
        imports.push((
            relative_import_specifier(helper_path, module_path.as_str()),
            bindings.clone(),
        ));
    }
    if let Some(source) = packed_named_import_statements(imports) {
        file.push_source(source);
    }
}

pub(crate) fn packed_named_import_statements(
    imports: impl IntoIterator<Item = (String, BTreeSet<BindingName>)>,
) -> Option<String> {
    let mut ordered = Vec::<(String, BTreeSet<BindingName>)>::new();
    let mut index_by_specifier = BTreeMap::<String, usize>::new();
    for (specifier, bindings) in imports {
        if bindings.is_empty() {
            continue;
        }
        if let Some(index) = index_by_specifier.get(&specifier).copied() {
            ordered[index].1.extend(bindings);
            continue;
        }
        index_by_specifier.insert(specifier.clone(), ordered.len());
        ordered.push((specifier, bindings));
    }
    if ordered.is_empty() {
        return None;
    }
    Some(
        ordered
            .iter()
            .map(|(specifier, bindings)| named_import_statement(bindings.iter(), specifier))
            .collect::<Vec<_>>()
            .join(""),
    )
}
