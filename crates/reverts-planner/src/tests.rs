use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::{
    RuntimeNamespaceExport, RuntimePrelude, RuntimePreludeBindingKind, RuntimePreludeSnippet,
};
use reverts_input::{
    InputBundle, InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
    PackageAttributionInput, PackageAttributionStatus, PackageSurfaceInput, ProjectInput,
    SourceFileInput, SourceSpan, SymbolInput,
};
use reverts_ir::{BindingName, BindingShape, BindingShapeSolution, ModuleId};
use reverts_model::{
    CompilerEvidence, CompilerKind, CompilerProfile, EnrichedProgram, ModuleCompilerProfile,
    ProgramModel,
};
use reverts_package::accepted_external_module_ids;

use super::external_adapters::{
    ExportMemberAdapterProofKind, compact_source_defines_callable_binding,
    export_member_adapter_proof,
};
use super::statement_parsers::parse_generated_named_export_statement;
use super::{
    CompilerPreservationAction, EmitPlan, ImportExportPlanner, PlannedFile, PlannerAnalysis,
    RuntimeReaderClusterContext, RuntimeReaderClusterMigration,
    RuntimeReaderClusterMigrationProposal, RuntimeSetterMigrationBlockerReason,
    RuntimeSourceReadIndex, SourceCompilerStrategy, coalesce_runtime_lazy_initializer_call_runs,
    compact_pure_static_runtime_literals, finalize_planned_file, inline_internal_setter_calls,
    inline_remaining_lazy_value_wrappers_allowing_assignments, lower_runtime_helpers,
    merge_same_owner_overlapping_reader_migrations, prune_orphan_runtime_bindings,
    purify_private_runtime_lazy_initializers,
};

fn enriched_from_rows(rows: InputRows) -> EnrichedProgram {
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    )
}

fn plan_from_rows(rows: InputRows) -> EmitPlan {
    ImportExportPlanner
        .plan_enriched_program(&enriched_from_rows(rows))
        .expect("fixture should normalize")
}

#[test]
fn runtime_source_module_import_scan_skips_helper_local_bindings() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "module.js",
        Some("function ub6() { return 1; }".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "ub6", "modules/ub6.ts").with_source_file(1));
    let enriched = enriched_from_rows(rows);
    let scan = super::scan_runtime_externalized_bindings(
        &enriched,
        "var ub6 = lazyValue(() => 1);\nub6();",
        &BTreeSet::new(),
        &BTreeSet::new(),
    );

    assert!(
        scan.source_module_imports.is_empty(),
        "helper-local folded bindings must not be imported from their omitted source module"
    );
}

#[test]
fn module_output_path_normalizes_virtual_bundle_ids_to_typescript_modules() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some("export const value = 1;".into()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(55), "esbuild:H0", "esbuild:H0").with_source_file(1),
    );
    let enriched = enriched_from_rows(rows);

    let path = super::module_output_path(&enriched, ModuleId(55))
        .expect("fixture module should have an output path");

    assert_eq!(path, "modules/55-esbuild-H0.ts");
}

#[test]
fn module_output_path_reroutes_node_modules_paths_off_the_special_directory() {
    // Preserved-layout bundles (Electron) keep `node_modules/<pkg>/...` paths.
    // An emitted source/adapter file under a literal `node_modules` directory
    // never compiles (tsc skips it) and its consumers' relative imports dangle,
    // so the path must be rerouted under `modules/` with the special segment
    // neutralized — never reintroducing a `node_modules` directory.
    let path = super::normalized_module_output_path(
        ModuleId(43),
        "Contents/Resources/app/node_modules/ws/lib/event-target.ts",
    );

    assert!(
        !path.split('/').any(|segment| segment == "node_modules"),
        "rerouted path must not contain a node_modules directory segment: {path}"
    );
    assert!(
        path.starts_with("modules/43-"),
        "node_modules path should reroute under modules/: {path}"
    );
    assert!(path.ends_with(".ts"), "{path}");
}

#[test]
fn rewritable_externalized_package_init_shim_calls_are_erased() {
    let mut shims = BTreeSet::from([BindingName::new("packageInit")]);
    let rewritten = super::erase_rewritable_package_init_shim_calls(
        "packageInit();\nfunction helper() { return packageInit(); }\n",
        &mut shims,
    );

    assert!(shims.is_empty(), "{rewritten}");
    assert!(!rewritten.contains("packageInit"), "{rewritten}");
    assert!(rewritten.contains("function helper() { return void 0; }"));
}

#[test]
fn top_level_statement_spans_use_parser_statement_boundaries() {
    let source = "function first() { return 1; }function second() { return 2; }\nvar third = 3;\n";

    let spans = super::top_level_statement_spans(source);

    assert_eq!(spans.len(), 3);
    assert_eq!(
        &source[spans[0].0..spans[0].1],
        "function first() { return 1; }"
    );
    assert_eq!(
        &source[spans[1].0..spans[1].1],
        "function second() { return 2; }"
    );
    assert_eq!(&source[spans[2].0..spans[2].1], "var third = 3;");
}

#[test]
fn identifier_read_scans_return_none_on_unparseable_source() {
    // Generated source can contain shapes the TypeScript parser refuses
    // (e.g. JSX expressions using the comma operator surfaced from real
    // bundles). The fact extractor must report None so safety-check
    // callers can declare the rename unsafe rather than admitting on an
    // empty fact set.
    assert!(super::try_identifier_read_facts_in_source("function entry(").is_none());
    assert!(super::identifier_read_facts_in_source("function entry(").is_empty());
}

fn planned_source(plan: &EmitPlan, path: &str) -> String {
    plan.files
        .iter()
        .find(|file| file.path == path)
        .unwrap_or_else(|| panic!("{path} should be planned"))
        .body
        .join("\n")
}

fn planned_source_opt(plan: &EmitPlan, path: &str) -> Option<String> {
    plan.files
        .iter()
        .find(|file| file.path == path)
        .map(|file| file.body.join("\n"))
}

#[test]
fn runtime_binding_graph_prunes_unreachable_private_function_closure() {
    let source = "\
function entry() { return dep(); }\n\
function dep() { return 1; }\n\
function orphan() { return dep(); }\n\
function orphanDep() { return 2; }\n";
    let roots = BTreeSet::from([BindingName::new("entry")]);

    let pruned = prune_orphan_runtime_bindings(source, &roots);

    assert!(pruned.source.contains("function entry()"));
    assert!(pruned.source.contains("function dep()"));
    assert!(!pruned.source.contains("function orphan()"));
    assert!(!pruned.source.contains("function orphanDep()"));
    assert_eq!(
        pruned.dropped_bindings,
        BTreeSet::from([BindingName::new("orphan"), BindingName::new("orphanDep")])
    );
}

#[test]
fn runtime_binding_graph_preserves_unparseable_source() {
    let pruned = prune_orphan_runtime_bindings(
        "function entry(",
        &BTreeSet::from([BindingName::new("entry")]),
    );

    assert_eq!(pruned.source, "function entry(");
    assert!(pruned.dropped_bindings.is_empty());
}

#[test]
fn runtime_binding_graph_keeps_side_effect_dependencies() {
    let source = "\
function dep() { return 1; }\n\
function orphan() { return 2; }\n\
dep();\n";
    let pruned = prune_orphan_runtime_bindings(source, &BTreeSet::new());

    assert!(pruned.source.contains("function dep()"));
    assert!(!pruned.source.contains("function orphan()"));
    assert!(pruned.source.contains("dep();"));
}

#[test]
fn runtime_binding_graph_does_not_prune_effectful_var_initializer() {
    let source = "\
function dep() { return 1; }\n\
var orphan = build(dep());\n";
    let pruned = prune_orphan_runtime_bindings(source, &BTreeSet::new());

    assert!(pruned.source.contains("function dep()"));
    assert!(pruned.source.contains("var orphan = build(dep());"));
    assert!(pruned.dropped_bindings.is_empty());
}

#[test]
fn runtime_binding_graph_prunes_pure_var_initializer_and_dependencies() {
    let source = "\
function dep() { return 1; }\n\
var orphan = () => dep();\n";
    let pruned = prune_orphan_runtime_bindings(source, &BTreeSet::new());

    assert!(!pruned.source.contains("function dep()"));
    assert!(!pruned.source.contains("var orphan"));
    assert_eq!(
        pruned.dropped_bindings,
        BTreeSet::from([BindingName::new("dep"), BindingName::new("orphan")])
    );
}

#[test]
fn runtime_lazy_call_run_coalescing_compacts_only_lazy_body_top_level_calls() {
    let source = "\
var init = lazyValue(() => {\n\
\ta();\n\
\tb();\n\
\tvoid 0;\n\
\tvalue = 1;\n\
\tc(arg);\n\
\td();\n\
\te();\n\
});\n\
function keep() {\n\
\ta();\n\
\tb();\n\
}\n";

    let compacted = coalesce_runtime_lazy_initializer_call_runs(source);

    assert!(compacted.contains("\ta(), b(), void 0;\n\tvalue = 1;"));
    assert!(compacted.contains("\td(), e();"));
    assert!(compacted.contains("function keep() {\n\ta();\n\tb();\n}"));
    assert!(compacted.contains("\tc(arg);"));
}

#[test]
fn runtime_static_literal_compaction_minifies_only_pure_assignment_literals() {
    let source = "\
var init = lazyValue(() => {\n\
\tconfig = {\n\
\t\tname: 'alpha beta',\n\
\t\tflags: [\n\
\t\t\t'a',\n\
\t\t\t'b'\n\
\t\t]\n\
\t};\n\
\titems = [\n\
\t\t1,\n\
\t\t2,\n\
\t\t3,\n\
\t\t4\n\
\t];\n\
\tkept = build({\n\
\t\tvalue: 1\n\
\t});\n\
});\n";

    let compacted = compact_pure_static_runtime_literals(source);

    assert!(compacted.contains("config = {name:'alpha beta',flags:['a','b']}"));
    assert!(compacted.contains("items = [1,2,3,4]"));
    assert!(
        compacted.contains("kept = build({\n\t\tvalue: 1\n\t});"),
        "call argument literals are not assignment RHS values and must not be rewritten"
    );
}

#[test]
fn runtime_static_literal_compaction_allows_data_spreads_and_references() {
    let source = "\
var init = lazyValue(() => {\n\
\tbase = {\n\
\t\t'--all': 'none',\n\
\t\t'--tags': 'none'\n\
\t};\n\
\tconfig = {\n\
\t\t...base,\n\
\t\tmode: defaultMode,\n\
\t\tnested: {\n\
\t\t\tname: 'alpha',\n\
\t\t\tflags: [\n\
\t\t\t\t'A',\n\
\t\t\t\t'B'\n\
\t\t\t]\n\
\t\t}\n\
\t};\n\
});\n";

    let compacted = compact_pure_static_runtime_literals(source);

    assert!(
        compacted
            .contains("config = {...base,mode:defaultMode,nested:{name:'alpha',flags:['A','B']}}")
    );
}

#[test]
fn runtime_static_literal_compaction_preserves_regex_and_skips_arrow_values() {
    let source = "\
var init = lazyValue(() => {\n\
\tpatterns = [\n\
\t\t/a b+/g,\n\
\t\t'keep space',\n\
\t\t/another pattern/,\n\
\t\t'end'\n\
\t];\n\
\thandlers = {\n\
\t\tfirst: () => {\n\
\t\t\treturn 1;\n\
\t\t},\n\
\t\tsecond: value\n\
\t};\n\
});\n";

    let compacted = compact_pure_static_runtime_literals(source);

    assert!(compacted.contains("patterns = [/a b+/g,'keep space',/another pattern/,'end']"));
    assert!(
        compacted.contains("handlers = {\n\t\tfirst: () => {"),
        "function-bearing object literals stay expanded"
    );
}

#[test]
fn emit_plan_push_file_is_data_only_without_finalization_pass() {
    let mut file = PlannedFile::new("modules/consumer.ts");
    file.push_source("import { beta } from './dep.js';");
    file.push_source("import { alpha } from './dep.js';");

    let mut plan = EmitPlan::default();
    plan.push_file(file);
    let source = planned_source(&plan, "modules/consumer.ts");

    assert_eq!(source.matches("from './dep.js'").count(), 2);
    assert!(source.contains("import { beta } from './dep.js';"));
    assert!(source.contains("import { alpha } from './dep.js';"));
}

#[test]
fn emit_plan_coalesces_duplicate_generated_named_imports() {
    let mut file = PlannedFile::new("modules/consumer.ts");
    file.push_source("import { beta } from './runtime/source-1-helpers.js';");
    file.push_source("import { alpha } from './runtime/source-1-helpers.js';");
    file.push_source("import { local } from './local.js';");
    file.push_source("console.log(alpha, beta, local);");

    let mut plan = EmitPlan::default();
    finalize_planned_file(&mut file);
    plan.push_file(file);
    let source = planned_source(&plan, "modules/consumer.ts");

    assert!(source.contains("import { alpha, beta } from './runtime/source-1-helpers.js';"));
    assert_eq!(
        source
            .matches("from './runtime/source-1-helpers.js'")
            .count(),
        1
    );
    assert!(source.contains("import { local } from './local.js';"));
}

#[test]
fn lazy_helper_imports_use_shared_runtime_lazy_file() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::application(
        ModuleId(1),
        "entry",
        "modules/entry.ts",
    ));
    let program = enriched_from_rows(rows);
    let mut file = PlannedFile::new("modules/entry.ts");
    let mut planned_bindings = BTreeSet::new();

    super::emit_lowered_runtime_helper_import(super::LoweredRuntimeHelperImportArgs {
        program: &program,
        module_id: ModuleId(1),
        module_path: "modules/entry.ts",
        file: &mut file,
        planned_bindings: &mut planned_bindings,
        source_file_id: 1,
        remaining_runtime_helpers: &BTreeSet::new(),
        written_runtime_helpers: &BTreeSet::new(),
        lazy_helper_names: &["lazyValue"],
    });
    finalize_planned_file(&mut file);

    let source = file.body.join("\n");
    assert!(source.contains("import { lazyValue } from './runtime/lazy.js';"));
    assert!(!source.contains("source-1-helpers"));
}

#[test]
fn emit_plan_coalesces_generated_default_and_named_imports() {
    let mut file = PlannedFile::new("modules/consumer.ts");
    file.push_source("import defaultValue from './dep.js';");
    file.push_source("import { beta } from './dep.js';");
    file.push_source("import { alpha } from './dep.js';");
    file.push_source("console.log(defaultValue, alpha, beta);");

    let mut plan = EmitPlan::default();
    finalize_planned_file(&mut file);
    plan.push_file(file);
    let source = planned_source(&plan, "modules/consumer.ts");

    assert!(source.contains("import defaultValue, { alpha, beta } from './dep.js';"));
    assert_eq!(source.matches("from './dep.js'").count(), 1);
}

#[test]
fn runtime_import_declaration_coalescing_merges_default_and_named_groups() {
    let source = concat!(
        "import pathDefault from 'path';\n",
        "import { join as pathJoin } from 'path';\n",
        "import { sep } from 'path';\n",
        "import * as fsNS from 'fs';\n",
        "function use() { return [pathDefault, pathJoin, sep, fsNS]; }\n",
    );

    let coalesced = super::coalesce_top_level_import_declarations(source);

    assert!(coalesced.contains("import pathDefault, { join as pathJoin, sep } from 'path';"));
    assert_eq!(coalesced.matches("from 'path'").count(), 1);
    assert!(coalesced.contains("import * as fsNS from 'fs';"));
    assert!(coalesced.contains("function use()"));
}

#[test]
fn runtime_import_declaration_coalescing_sees_imports_after_declarations() {
    let source = concat!(
        "function before() { return 1; }\n",
        "import { join as pathJoin } from 'path';\n",
        "function middle() { return pathJoin; }\n",
        "import { sep as pathSep } from 'path';\n",
        "function after() { return pathSep; }\n",
    );

    let coalesced = super::coalesce_top_level_import_declarations(source);

    assert!(coalesced.contains("import { join as pathJoin, sep as pathSep } from 'path';"));
    assert_eq!(coalesced.matches("from 'path'").count(), 1);
    assert!(coalesced.contains("function before()"));
    assert!(coalesced.contains("function middle()"));
    assert!(coalesced.contains("function after()"));
}

#[test]
fn runtime_import_declaration_coalescing_preserves_multiple_defaults() {
    let source = concat!(
        "import cryptoA, { createHash as hashA } from 'crypto';\n",
        "function first() { return cryptoA; }\n",
        "import cryptoB from 'crypto';\n",
        "import { randomBytes as randomBytesB } from 'crypto';\n",
        "function second() { return [hashA, cryptoB, randomBytesB]; }\n",
    );

    let coalesced = super::coalesce_top_level_import_declarations(source);

    assert!(coalesced.contains(
        "import cryptoA, { createHash as hashA, randomBytes as randomBytesB } from 'crypto';"
    ));
    assert!(coalesced.contains("import cryptoB from 'crypto';"));
    assert_eq!(coalesced.matches("from 'crypto'").count(), 2);
    assert!(coalesced.contains("function first()"));
    assert!(coalesced.contains("function second()"));
}

#[test]
fn runtime_import_declaration_coalescing_preserves_same_line_prelude() {
    let source = concat!(
        "var state;import pathDefault from 'path';\n",
        "function useDefault() { return pathDefault; }\n",
        "import { join as pathJoin } from 'path';\n",
    );

    let coalesced = super::coalesce_top_level_import_declarations(source);

    assert!(coalesced.contains("var state;import pathDefault, { join as pathJoin } from 'path';"));
    assert!(!coalesced.contains("var state;\nimport"));
}

#[test]
fn runtime_import_trivia_compaction_only_touches_top_level_whitespace() {
    let source = concat!(
        "import a from 'a';\n\n",
        "import b from 'b';\n\n",
        "function f() { return `keep\\n\\nblank`; }\n\n",
        "import { c } from 'c';\n",
    );

    let compacted = super::import_coalesce::compact_top_level_import_trivia(source);

    assert!(compacted.contains("import a from 'a';import b from 'b';"));
    assert!(compacted.contains("`keep\\n\\nblank`"));
    assert!(compacted.contains("}\nimport { c } from 'c';"));
}

#[test]
fn runtime_setter_declarations_are_batched_without_newlines() {
    let declarations = super::runtime_helper_setter_declarations(&BTreeSet::from([
        BindingName::new("left"),
        BindingName::new("right"),
    ]));

    assert_eq!(declarations.lines().count(), 1);
    assert!(declarations.contains(
        "function __reverts_set_left(value) { return left = value; }function __reverts_set_right(value) { return right = value; }"
    ));
    super::collect_top_level_statement_facts(&declarations, None, super::ParseGoal::TypeScript)
        .expect("batched setter declarations must remain parseable");
}

#[test]
fn packed_named_import_statements_preserve_order_and_merge_duplicates() {
    let source = super::package_runtime::packed_named_import_statements([
        (
            "./first.js".to_string(),
            BTreeSet::from([BindingName::new("beta")]),
        ),
        (
            "./second.js".to_string(),
            BTreeSet::from([BindingName::new("gamma")]),
        ),
        (
            "./first.js".to_string(),
            BTreeSet::from([BindingName::new("alpha")]),
        ),
    ])
    .expect("imports should be emitted");

    assert_eq!(source.lines().count(), 1);
    assert_eq!(
        source,
        "import { alpha, beta } from './first.js';import { gamma } from './second.js';"
    );
    super::collect_top_level_statement_facts(&source, None, super::ParseGoal::TypeScript)
        .expect("packed import declarations must remain parseable");
}

#[test]
fn binding_owner_plan_applies_global_precedence() {
    let source_file_id = 1;
    let migrated = BindingName::new("migratedState");
    let migrated_reader = BindingName::new("readMigratedState");
    let prelude_imported = BindingName::new("pathJoin");
    let package_owned = BindingName::new("packageHelper");
    let runtime_owned = BindingName::new("sharedRuntime");
    let package_owner = super::package_runtime::PackageRuntimeOwner {
        name: "pkg".to_string(),
        version: "1.0.0".to_string(),
    };

    let mut migrations = super::RuntimeVarMigrationPlan::default();
    migrations.insert(
        migrated.clone(),
        super::runtime_var_migration::RuntimeVarMigration {
            owner_module: ModuleId(7),
            source_file_id,
            extra_snippets: BTreeSet::from([migrated_reader.clone()]),
            extra_namespace_exports: BTreeSet::new(),
            extra_runtime_deps: BTreeSet::new(),
            extra_runtime_setter_deps: BTreeSet::new(),
            extra_runtime_dep_aliases: BTreeMap::new(),
            extra_source_deps: BTreeMap::new(),
            extra_runtime_reexport_source_deps: BTreeMap::new(),
            extra_noop_deps: BTreeSet::new(),
            initializer: None,
        },
    );
    let direct_import = super::RuntimePreludeDirectImport {
        source: "path".to_string(),
        snippet_source: "import { join as pathJoin } from 'path';".to_string(),
        snippet_byte_start: 0,
        kind: super::RuntimePreludeDirectImportKind::Named {
            imported: "join".to_string(),
        },
    };
    let direct_imports = BTreeMap::from([(
        source_file_id,
        BTreeMap::from([
            (prelude_imported.clone(), direct_import.clone()),
            // A direct-import-looking binding that was later migrated must
            // be owned by its module; migration wins over prelude/package
            // routing in the global owner table.
            (migrated.clone(), direct_import.clone()),
        ]),
    )]);
    let package_islands = super::package_runtime::PackageRuntimeIslandPlan {
        owners_by_binding: BTreeMap::from([
            (
                (source_file_id, package_owned.clone()),
                package_owner.clone(),
            ),
            (
                (source_file_id, prelude_imported.clone()),
                package_owner.clone(),
            ),
        ]),
    };

    let owners =
        super::BindingOwnerPlan::from_parts(&migrations, &direct_imports, &package_islands);

    assert_eq!(
        owners.module_owner(source_file_id, &migrated),
        Some(ModuleId(7))
    );
    assert_eq!(
        owners.module_owner(source_file_id, &migrated_reader),
        Some(ModuleId(7))
    );
    assert_eq!(
        owners.package_runtime_owner(source_file_id, &package_owned),
        Some(&package_owner)
    );
    assert!(matches!(
        owners.owner_for(source_file_id, &prelude_imported),
        super::BindingOwner::PreludeImport(_)
    ));
    assert!(matches!(
        owners.owner_for(source_file_id, &runtime_owned),
        super::BindingOwner::Runtime
    ));

    let partition = super::partition_runtime_owner_bindings(
        &owners,
        source_file_id,
        ModuleId(3),
        BTreeSet::from([
            migrated.clone(),
            prelude_imported.clone(),
            package_owned.clone(),
            runtime_owned.clone(),
        ]),
    );
    assert_eq!(
        partition.direct_imports.get(&ModuleId(7)),
        Some(&BTreeSet::from([migrated.clone()]))
    );
    assert!(
        partition
            .direct_prelude_imports
            .contains_key(&prelude_imported)
    );
    assert!(partition.runtime_bindings.contains(&package_owned));
    assert!(partition.runtime_bindings.contains(&runtime_owned));
}

#[test]
fn global_owner_rebuild_selects_closed_runtime_snippet_component() {
    let source = "\
function ownedA() { return ownedB(); }\n\
function ownedB() { return 1; }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("ownedB"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                snippet("function ownedA() { return ownedB(); }"),
            ),
            (
                BindingName::new("ownedB"),
                snippet("function ownedB() { return 1; }"),
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let candidate_owners = BTreeMap::from([
        (BindingName::new("ownedA"), ModuleId(7)),
        (BindingName::new("ownedB"), ModuleId(7)),
    ]);
    let owner_available_bindings = BTreeMap::from([(ModuleId(7), BTreeSet::new())]);

    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &candidate_owners,
        &owner_available_bindings,
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    assert_eq!(
        selected.keys().cloned().collect::<BTreeSet<_>>(),
        candidate_owners.keys().cloned().collect::<BTreeSet<_>>()
    );
    assert!(selected.values().all(|migration| {
        migration.owner_module == ModuleId(7) && migration.extra_runtime_deps.is_empty()
    }));
}

#[test]
fn global_owner_rebuild_rejects_runtime_reader_outside_component() {
    let source = "\
function ownedA() { return ownedB(); }\n\
function ownedB() { return 1; }\n\
function runtimeReader() { return ownedA(); }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("ownedB"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("runtimeReader"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                snippet("function ownedA() { return ownedB(); }"),
            ),
            (
                BindingName::new("ownedB"),
                snippet("function ownedB() { return 1; }"),
            ),
            (
                BindingName::new("runtimeReader"),
                snippet("function runtimeReader() { return ownedA(); }"),
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let candidate_owners = BTreeMap::from([
        (BindingName::new("ownedA"), ModuleId(7)),
        (BindingName::new("ownedB"), ModuleId(7)),
    ]);
    let owner_available_bindings = BTreeMap::from([(ModuleId(7), BTreeSet::new())]);

    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &candidate_owners,
        &owner_available_bindings,
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    assert!(selected.is_empty());
}

#[test]
fn global_owner_rebuild_allows_lazy_safe_retained_function_reader() {
    let source = "\
function ownedA() { return ownedB(); }\n\
function ownedB() { return 1; }\n\
function runtimeReader() { return ownedA(); }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("ownedB"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("runtimeReader"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                snippet("function ownedA() { return ownedB(); }"),
            ),
            (
                BindingName::new("ownedB"),
                snippet("function ownedB() { return 1; }"),
            ),
            (
                BindingName::new("runtimeReader"),
                snippet("function runtimeReader() { return ownedA(); }"),
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([
            (BindingName::new("ownedA"), ModuleId(7)),
            (BindingName::new("ownedB"), ModuleId(7)),
        ]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::from([(
            ModuleId(7),
            super::RuntimeReaderOwnerRuntimeState {
                source: "var ownerInit = lazyValue(() => {});".to_string(),
                remaining_helpers: BTreeSet::new(),
                written_helpers: BTreeSet::new(),
                uses_lazy_module: false,
                uses_lazy_value: true,
                can_localize_lazy_value: false,
            },
        )]),
    );

    assert_eq!(
        selected.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([BindingName::new("ownedA"), BindingName::new("ownedB")])
    );
}

#[test]
fn global_owner_rebuild_rejects_eager_retained_function_reader() {
    let source = "\
function ownedA() { return ownedB(); }\n\
function ownedB() { return 1; }\n\
function runtimeReader() { return ownedA(); }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("ownedB"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("runtimeReader"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                snippet("function ownedA() { return ownedB(); }"),
            ),
            (
                BindingName::new("ownedB"),
                snippet("function ownedB() { return 1; }"),
            ),
            (
                BindingName::new("runtimeReader"),
                snippet("function runtimeReader() { return ownedA(); }"),
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let folded_chunks = vec![super::RuntimeFoldedSourceChunk {
        byte_start: source.len() as u32,
        source: "runtimeReader();".to_string(),
    }];
    let read_index = super::runtime_source_read_index(&prelude, &folded_chunks);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([
            (BindingName::new("ownedA"), ModuleId(7)),
            (BindingName::new("ownedB"), ModuleId(7)),
        ]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::from([(
            ModuleId(7),
            super::RuntimeReaderOwnerRuntimeState {
                source: "var ownerInit = lazyValue(() => {});".to_string(),
                remaining_helpers: BTreeSet::new(),
                written_helpers: BTreeSet::new(),
                uses_lazy_module: false,
                uses_lazy_value: true,
                can_localize_lazy_value: false,
            },
        )]),
    );

    assert!(selected.is_empty());
}

#[test]
fn global_owner_rebuild_imports_stable_runtime_deps() {
    let source = "\
function ownedA() { return runtimeDep(); }\n\
function runtimeDep() { return 1; }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("runtimeDep"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                snippet("function ownedA() { return runtimeDep(); }"),
            ),
            (
                BindingName::new("runtimeDep"),
                snippet("function runtimeDep() { return 1; }"),
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let candidate_owners = BTreeMap::from([(BindingName::new("ownedA"), ModuleId(7))]);
    let owner_available_bindings = BTreeMap::from([(
        ModuleId(7),
        BTreeSet::from([BindingName::new("runtimeDep")]),
    )]);

    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &candidate_owners,
        &owner_available_bindings,
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    let migration = selected
        .get(&BindingName::new("ownedA"))
        .expect("owned snippet should move with a runtime import");
    assert_eq!(
        migration.extra_runtime_deps,
        BTreeSet::from([BindingName::new("runtimeDep")])
    );
    assert_eq!(
        migration
            .extra_runtime_dep_aliases
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([BindingName::new("runtimeDep")])
    );
}

#[test]
fn global_owner_rebuild_localizes_noop_runtime_deps() {
    let source = "\
function ownedA() { return Promise.resolve().catch(noop); }\n\
function noop() {}\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("noop"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                snippet("function ownedA() { return Promise.resolve().catch(noop); }"),
            ),
            (BindingName::new("noop"), snippet("function noop() {}")),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([(BindingName::new("ownedA"), ModuleId(7))]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    let migration = selected
        .get(&BindingName::new("ownedA"))
        .expect("owned snippet should move with a local noop");
    assert!(migration.extra_runtime_deps.is_empty());
    assert_eq!(
        migration.extra_noop_deps,
        BTreeSet::from([BindingName::new("noop")])
    );
}

#[test]
fn global_owner_rebuild_propagates_owner_through_single_reader_closure() {
    let source = "\
function ownedA() { return bridge(); }\n\
function bridge() { return leaf(); }\n\
function leaf() { return 1; }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("bridge"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("leaf"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                snippet("function ownedA() { return bridge(); }"),
            ),
            (
                BindingName::new("bridge"),
                snippet("function bridge() { return leaf(); }"),
            ),
            (
                BindingName::new("leaf"),
                snippet("function leaf() { return 1; }"),
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let eligible = BTreeSet::from([
        BindingName::new("ownedA"),
        BindingName::new("bridge"),
        BindingName::new("leaf"),
    ]);
    let mut candidate_owners = BTreeMap::from([(BindingName::new("ownedA"), ModuleId(7))]);

    super::propagate_runtime_reader_owned_snippet_candidates(
        &prelude,
        &read_index,
        &eligible,
        &mut candidate_owners,
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeSet::new(),
    );

    assert_eq!(
        candidate_owners,
        BTreeMap::from([
            (BindingName::new("ownedA"), ModuleId(7)),
            (BindingName::new("bridge"), ModuleId(7)),
            (BindingName::new("leaf"), ModuleId(7)),
        ])
    );
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &candidate_owners,
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    assert_eq!(selected.keys().cloned().collect::<BTreeSet<_>>(), eligible);
}

#[test]
fn global_owner_rebuild_does_not_duplicate_folded_owner_local_definitions() {
    let source = "\
function ownedA() { return localDep(); }\n\
function localDep() { return 1; }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("localDep"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                snippet("function ownedA() { return localDep(); }"),
            ),
            (
                BindingName::new("localDep"),
                snippet("function localDep() { return 1; }"),
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let mut candidate_owners = BTreeMap::from([(BindingName::new("ownedA"), ModuleId(7))]);

    super::propagate_runtime_reader_owned_snippet_candidates(
        &prelude,
        &read_index,
        &BTreeSet::from([BindingName::new("ownedA"), BindingName::new("localDep")]),
        &mut candidate_owners,
        &BTreeMap::from([(ModuleId(7), BTreeSet::from([BindingName::new("localDep")]))]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::from([BindingName::new("localDep")]))]),
        &BTreeSet::from([ModuleId(7)]),
    );

    assert_eq!(
        candidate_owners,
        BTreeMap::from([(BindingName::new("ownedA"), ModuleId(7))]),
        "folded owners still emit their local source definitions, so propagation must not re-inject them"
    );
}

#[test]
fn global_owner_rebuild_keeps_source_file_local_symbol_owners_distinct() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "a.js",
        Some("function same() { return 1; }".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "b.js",
        Some("function same() { return 2; }".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(10), "a", "modules/a.ts").with_source_file(1));
    rows.modules
        .push(ModuleInput::application(ModuleId(20), "b", "modules/b.ts").with_source_file(2));
    rows.symbols.push(SymbolInput::new(ModuleId(10), "same"));
    rows.symbols.push(SymbolInput::new(ModuleId(20), "same"));
    let enriched = enriched_from_rows(rows);

    let by_source = super::runtime_owner_definition_modules_by_source(&enriched, &BTreeSet::new());
    let global = super::runtime_owner_definition_modules(&enriched, &BTreeSet::new());

    assert_eq!(
        by_source
            .get(&1)
            .and_then(|owners| owners.get(&BindingName::new("same")))
            .copied()
            .flatten(),
        Some(ModuleId(10))
    );
    assert_eq!(
        by_source
            .get(&2)
            .and_then(|owners| owners.get(&BindingName::new("same")))
            .copied()
            .flatten(),
        Some(ModuleId(20))
    );
    assert_eq!(global.get(&BindingName::new("same")), Some(&None));
}

#[test]
fn global_owner_rebuild_allows_acyclic_cross_owner_runtime_deps() {
    let source = "\
function ownedA() { return ownedB(); }\n\
function ownedB() { return 1; }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("ownedB"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                snippet("function ownedA() { return ownedB(); }"),
            ),
            (
                BindingName::new("ownedB"),
                snippet("function ownedB() { return 1; }"),
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let candidate_owners = BTreeMap::from([
        (BindingName::new("ownedA"), ModuleId(7)),
        (BindingName::new("ownedB"), ModuleId(8)),
    ]);
    let owner_available_bindings = BTreeMap::new();

    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &candidate_owners,
        &owner_available_bindings,
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    assert_eq!(
        selected.keys().cloned().collect::<BTreeSet<_>>(),
        candidate_owners.keys().cloned().collect::<BTreeSet<_>>()
    );
    assert_eq!(
        selected
            .get(&BindingName::new("ownedA"))
            .expect("A should move")
            .extra_runtime_deps,
        BTreeSet::from([BindingName::new("ownedB")])
    );
}

#[test]
fn global_owner_rebuild_rejects_cross_owner_runtime_cycles() {
    let source = "\
function ownedA() { return ownedB(); }\n\
function ownedB() { return ownedA(); }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("ownedB"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ownedA"),
                snippet("function ownedA() { return ownedB(); }"),
            ),
            (
                BindingName::new("ownedB"),
                snippet("function ownedB() { return ownedA(); }"),
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let candidate_owners = BTreeMap::from([
        (BindingName::new("ownedA"), ModuleId(7)),
        (BindingName::new("ownedB"), ModuleId(8)),
    ]);

    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &candidate_owners,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    assert!(selected.is_empty());
}

#[test]
fn global_owner_rebuild_pins_smaller_cyclic_cross_owner_dep() {
    let source = "\
function ownedBig() {\n\
  const value = ownedSmall();\n\
  return value + 1;\n\
}\n\
function ownedSmall() { return 1; }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ownedBig"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("ownedSmall"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ownedBig"),
                snippet(
                    "function ownedBig() {\n  const value = ownedSmall();\n  return value + 1;\n}",
                ),
            ),
            (
                BindingName::new("ownedSmall"),
                snippet("function ownedSmall() { return 1; }"),
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([
            (BindingName::new("ownedBig"), ModuleId(7)),
            (BindingName::new("ownedSmall"), ModuleId(8)),
        ]),
        &BTreeMap::new(),
        &BTreeMap::from([(ModuleId(8), BTreeSet::from([ModuleId(7)]))]),
        &BTreeMap::new(),
    );

    assert!(selected.contains_key(&BindingName::new("ownedBig")));
    assert!(!selected.contains_key(&BindingName::new("ownedSmall")));
    assert_eq!(
        selected
            .get(&BindingName::new("ownedBig"))
            .expect("larger owner snippet should move")
            .extra_runtime_deps,
        BTreeSet::from([BindingName::new("ownedSmall")])
    );
}

#[test]
fn global_owner_rebuild_infers_namespace_target_owner_from_namespace_object() {
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: "var ns = {}; function exportedOnly() { return 1; }".to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ns"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("exportedOnly"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ns"),
                RuntimePreludeSnippet {
                    source: "var ns = {};".to_string(),
                    byte_start: 0,
                    sub_snippets: Vec::new(),
                },
            ),
            (
                BindingName::new("exportedOnly"),
                RuntimePreludeSnippet {
                    source: "function exportedOnly() { return 1; }".to_string(),
                    byte_start: 13,
                    sub_snippets: Vec::new(),
                },
            ),
        ]),
        namespace_exports: vec![RuntimeNamespaceExport {
            namespace: BindingName::new("ns"),
            helper: BindingName::new("__export"),
            exports: BTreeMap::from([(
                "exportedOnly".to_string(),
                BindingName::new("exportedOnly"),
            )]),
            byte_start: 55,
        }],
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let target_owners = super::runtime_namespace_target_owners(
        &read_index,
        &BTreeMap::from([(BindingName::new("ns"), Some(ModuleId(7)))]),
    );

    assert_eq!(
        target_owners.get(&BindingName::new("exportedOnly")),
        Some(&Some(ModuleId(7)))
    );
}

#[test]
fn global_owner_rebuild_infers_adjacent_owner_between_matching_known_neighbors() {
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: "\
function before() { return 1; }\n\
function missing() { return before(); }\n\
function after() { return missing(); }\n"
            .to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("before"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("missing"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("after"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("before"),
                RuntimePreludeSnippet {
                    source: "function before() { return 1; }".to_string(),
                    byte_start: 0,
                    sub_snippets: Vec::new(),
                },
            ),
            (
                BindingName::new("missing"),
                RuntimePreludeSnippet {
                    source: "function missing() { return before(); }".to_string(),
                    byte_start: 32,
                    sub_snippets: Vec::new(),
                },
            ),
            (
                BindingName::new("after"),
                RuntimePreludeSnippet {
                    source: "function after() { return missing(); }".to_string(),
                    byte_start: 75,
                    sub_snippets: Vec::new(),
                },
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let eligible = BTreeSet::from([
        BindingName::new("before"),
        BindingName::new("missing"),
        BindingName::new("after"),
    ]);
    let inferred = super::runtime_adjacent_snippet_owners(
        &prelude,
        &eligible,
        &BTreeMap::from([
            (BindingName::new("before"), ModuleId(7)),
            (BindingName::new("after"), ModuleId(7)),
        ]),
    );

    assert_eq!(
        inferred.get(&BindingName::new("missing")),
        Some(&ModuleId(7))
    );

    let ambiguous = super::runtime_adjacent_snippet_owners(
        &prelude,
        &eligible,
        &BTreeMap::from([
            (BindingName::new("before"), ModuleId(7)),
            (BindingName::new("after"), ModuleId(8)),
        ]),
    );
    assert!(!ambiguous.contains_key(&BindingName::new("missing")));
}

#[test]
fn global_owner_rebuild_moves_namespace_export_with_target() {
    let source = "\
var ns = {};\n\
function ownedA() { return 1; }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ns"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("ownedA"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (BindingName::new("ns"), snippet("var ns = {};")),
            (
                BindingName::new("ownedA"),
                snippet("function ownedA() { return 1; }"),
            ),
        ]),
        namespace_exports: vec![RuntimeNamespaceExport {
            namespace: BindingName::new("ns"),
            helper: BindingName::new("__export"),
            exports: BTreeMap::from([("ownedA".to_string(), BindingName::new("ownedA"))]),
            byte_start: offset,
        }],
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let candidate_owners = BTreeMap::from([
        (BindingName::new("ns"), ModuleId(7)),
        (BindingName::new("ownedA"), ModuleId(7)),
    ]);
    let owner_available_bindings = BTreeMap::from([(ModuleId(7), BTreeSet::new())]);

    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &candidate_owners,
        &owner_available_bindings,
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    assert!(selected.contains_key(&BindingName::new("ownedA")));
    assert!(
        selected
            .get(&BindingName::new("ns"))
            .is_some_and(|migration| migration.moves_namespace_export)
    );
}

#[test]
fn global_owner_rebuild_allows_retained_namespace_reader_with_lazy_safe_owner() {
    let source = "\
var ns = {};\n\
class Owned {}\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ns"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("Owned"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (BindingName::new("ns"), snippet("var ns = {};")),
            (BindingName::new("Owned"), snippet("class Owned {}")),
        ]),
        namespace_exports: vec![RuntimeNamespaceExport {
            namespace: BindingName::new("ns"),
            helper: BindingName::new("__export"),
            exports: BTreeMap::from([("Owned".to_string(), BindingName::new("Owned"))]),
            byte_start: offset,
        }],
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([(BindingName::new("Owned"), ModuleId(7))]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::from([(
            ModuleId(7),
            super::RuntimeReaderOwnerRuntimeState {
                source: "var init = lazyValue(() => { runtimeDep(); });".to_string(),
                remaining_helpers: BTreeSet::from([
                    BindingName::new("lazyValue"),
                    BindingName::new("runtimeDep"),
                ]),
                written_helpers: BTreeSet::new(),
                uses_lazy_module: false,
                uses_lazy_value: true,
                can_localize_lazy_value: false,
            },
        )]),
    );

    assert!(
        selected.contains_key(&BindingName::new("Owned")),
        "namespace readers can remain in runtime when the runtime -> owner edge is lazy-safe"
    );
    assert!(!selected.contains_key(&BindingName::new("ns")));
}

#[test]
fn global_owner_rebuild_rejects_retained_namespace_reader_with_eager_owner_runtime_edge() {
    let source = "\
var ns = {};\n\
class Owned {}\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ns"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("Owned"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (BindingName::new("ns"), snippet("var ns = {};")),
            (BindingName::new("Owned"), snippet("class Owned {}")),
        ]),
        namespace_exports: vec![RuntimeNamespaceExport {
            namespace: BindingName::new("ns"),
            helper: BindingName::new("__export"),
            exports: BTreeMap::from([("Owned".to_string(), BindingName::new("Owned"))]),
            byte_start: offset,
        }],
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([(BindingName::new("Owned"), ModuleId(7))]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::from([(
            ModuleId(7),
            super::RuntimeReaderOwnerRuntimeState {
                source: "runtimeDep();".to_string(),
                remaining_helpers: BTreeSet::from([BindingName::new("runtimeDep")]),
                written_helpers: BTreeSet::new(),
                uses_lazy_module: false,
                uses_lazy_value: false,
                can_localize_lazy_value: false,
            },
        )]),
    );

    assert!(
        selected.is_empty(),
        "retained namespace readers must not synthesize eager runtime -> owner -> runtime cycles"
    );
}

#[test]
fn global_owner_rebuild_allows_folded_lazy_reader_to_import_owner() {
    let source = "function ownedA() { return 1; }\n";
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([(
            BindingName::new("ownedA"),
            RuntimePreludeBindingKind::SourceBacked,
        )]),
        snippets: BTreeMap::from([(
            BindingName::new("ownedA"),
            RuntimePreludeSnippet {
                source: "function ownedA() { return 1; }".to_string(),
                byte_start: 0,
                sub_snippets: Vec::new(),
            },
        )]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let folded_chunks = vec![super::RuntimeFoldedSourceChunk {
        byte_start: source.len() as u32,
        source: "var initUse = lazyValue(() => { ownedA(); });".to_string(),
    }];
    let read_index = super::runtime_source_read_index(&prelude, &folded_chunks);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([(BindingName::new("ownedA"), ModuleId(7))]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::from([(
            ModuleId(7),
            super::RuntimeReaderOwnerRuntimeState {
                source: "var ownerInit = lazyValue(() => {});".to_string(),
                remaining_helpers: BTreeSet::new(),
                written_helpers: BTreeSet::new(),
                uses_lazy_module: false,
                uses_lazy_value: true,
                can_localize_lazy_value: false,
            },
        )]),
    );

    assert!(
        selected.contains_key(&BindingName::new("ownedA")),
        "lazy folded reads can be routed through runtime -> owner imports"
    );
}

#[test]
fn global_owner_rebuild_allows_lazy_contained_owner_runtime_writes() {
    let source = "function ownedA() { return 1; }\n";
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([(
            BindingName::new("ownedA"),
            RuntimePreludeBindingKind::SourceBacked,
        )]),
        snippets: BTreeMap::from([(
            BindingName::new("ownedA"),
            RuntimePreludeSnippet {
                source: "function ownedA() { return 1; }".to_string(),
                byte_start: 0,
                sub_snippets: Vec::new(),
            },
        )]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let folded_chunks = vec![super::RuntimeFoldedSourceChunk {
        byte_start: source.len() as u32,
        source: "var initUse = lazyValue(() => { ownedA(); });".to_string(),
    }];
    let read_index = super::runtime_source_read_index(&prelude, &folded_chunks);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([(BindingName::new("ownedA"), ModuleId(7))]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::from([(
            ModuleId(7),
            super::RuntimeReaderOwnerRuntimeState {
                source: "var ownerInit = lazyValue(() => { runtimeWritten = 1; });".to_string(),
                remaining_helpers: BTreeSet::from([
                    BindingName::new("lazyValue"),
                    BindingName::new("runtimeWritten"),
                ]),
                written_helpers: BTreeSet::from([BindingName::new("runtimeWritten")]),
                uses_lazy_module: false,
                uses_lazy_value: true,
                can_localize_lazy_value: false,
            },
        )]),
    );

    assert!(
        selected.contains_key(&BindingName::new("ownedA")),
        "deferred lazy initializer writes do not make the runtime -> owner import eager"
    );
}

#[test]
fn global_owner_rebuild_rejects_eager_owner_runtime_writes() {
    let source = "function ownedA() { return 1; }\n";
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([(
            BindingName::new("ownedA"),
            RuntimePreludeBindingKind::SourceBacked,
        )]),
        snippets: BTreeMap::from([(
            BindingName::new("ownedA"),
            RuntimePreludeSnippet {
                source: "function ownedA() { return 1; }".to_string(),
                byte_start: 0,
                sub_snippets: Vec::new(),
            },
        )]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let folded_chunks = vec![super::RuntimeFoldedSourceChunk {
        byte_start: source.len() as u32,
        source: "var initUse = lazyValue(() => { ownedA(); });".to_string(),
    }];
    let read_index = super::runtime_source_read_index(&prelude, &folded_chunks);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([(BindingName::new("ownedA"), ModuleId(7))]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::from([(
            ModuleId(7),
            super::RuntimeReaderOwnerRuntimeState {
                source: "runtimeWritten = 1;".to_string(),
                remaining_helpers: BTreeSet::from([BindingName::new("runtimeWritten")]),
                written_helpers: BTreeSet::from([BindingName::new("runtimeWritten")]),
                uses_lazy_module: false,
                uses_lazy_value: false,
                can_localize_lazy_value: false,
            },
        )]),
    );

    assert!(
        selected.is_empty(),
        "eager writes would execute while importing the owner from runtime"
    );
}

#[test]
fn global_owner_rebuild_rejects_top_level_owner_lazy_write_call() {
    let source = "function ownedA() { return 1; }\n";
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([(
            BindingName::new("ownedA"),
            RuntimePreludeBindingKind::SourceBacked,
        )]),
        snippets: BTreeMap::from([(
            BindingName::new("ownedA"),
            RuntimePreludeSnippet {
                source: "function ownedA() { return 1; }".to_string(),
                byte_start: 0,
                sub_snippets: Vec::new(),
            },
        )]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let folded_chunks = vec![super::RuntimeFoldedSourceChunk {
        byte_start: source.len() as u32,
        source: "var initUse = lazyValue(() => { ownedA(); });".to_string(),
    }];
    let read_index = super::runtime_source_read_index(&prelude, &folded_chunks);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([(BindingName::new("ownedA"), ModuleId(7))]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::from([(
            ModuleId(7),
            super::RuntimeReaderOwnerRuntimeState {
                source: "var ownerInit = lazyValue(() => { runtimeWritten = 1; }); ownerInit();"
                    .to_string(),
                remaining_helpers: BTreeSet::from([
                    BindingName::new("lazyValue"),
                    BindingName::new("runtimeWritten"),
                ]),
                written_helpers: BTreeSet::from([BindingName::new("runtimeWritten")]),
                uses_lazy_module: false,
                uses_lazy_value: true,
                can_localize_lazy_value: false,
            },
        )]),
    );

    assert!(
        selected.is_empty(),
        "calling a lazy initializer at top level would eagerly trigger its runtime writes"
    );
}

#[test]
fn global_owner_rebuild_rejects_eager_folded_reader_import_owner_edge() {
    let source = "function ownedA() { return 1; }\n";
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([(
            BindingName::new("ownedA"),
            RuntimePreludeBindingKind::SourceBacked,
        )]),
        snippets: BTreeMap::from([(
            BindingName::new("ownedA"),
            RuntimePreludeSnippet {
                source: "function ownedA() { return 1; }".to_string(),
                byte_start: 0,
                sub_snippets: Vec::new(),
            },
        )]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let folded_chunks = vec![super::RuntimeFoldedSourceChunk {
        byte_start: source.len() as u32,
        source: "ownedA();".to_string(),
    }];
    let read_index = super::runtime_source_read_index(&prelude, &folded_chunks);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([(BindingName::new("ownedA"), ModuleId(7))]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::from([(
            ModuleId(7),
            super::RuntimeReaderOwnerRuntimeState {
                source: "var ownerInit = lazyValue(() => {});".to_string(),
                remaining_helpers: BTreeSet::new(),
                written_helpers: BTreeSet::new(),
                uses_lazy_module: false,
                uses_lazy_value: true,
                can_localize_lazy_value: false,
            },
        )]),
    );

    assert!(
        selected.is_empty(),
        "eager folded calls must stay in runtime to avoid evaluation cycles"
    );
}

#[test]
fn global_owner_rebuild_allows_folded_lazy_namespace_reader_to_import_owner() {
    let source = "\
var ns = {};\n\
function target() { return 1; }\n";
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("ns"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("target"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("ns"),
                RuntimePreludeSnippet {
                    source: "var ns = {};".to_string(),
                    byte_start: 0,
                    sub_snippets: Vec::new(),
                },
            ),
            (
                BindingName::new("target"),
                RuntimePreludeSnippet {
                    source: "function target() { return 1; }".to_string(),
                    byte_start: "var ns = {};\n".len() as u32,
                    sub_snippets: Vec::new(),
                },
            ),
        ]),
        namespace_exports: vec![RuntimeNamespaceExport {
            namespace: BindingName::new("ns"),
            helper: BindingName::new("__export"),
            exports: BTreeMap::from([("target".to_string(), BindingName::new("target"))]),
            byte_start: source.len() as u32,
        }],
        entrypoint: None,
    };
    let folded_chunks = vec![super::RuntimeFoldedSourceChunk {
        byte_start: source.len() as u32 + 1,
        source: "var initUse = lazyValue(() => ns);".to_string(),
    }];
    let read_index = super::runtime_source_read_index(&prelude, &folded_chunks);
    let selected = super::closed_global_owned_runtime_snippets(
        &prelude,
        &read_index,
        &BTreeMap::from([
            (BindingName::new("ns"), ModuleId(7)),
            (BindingName::new("target"), ModuleId(7)),
        ]),
        &BTreeMap::from([(ModuleId(7), BTreeSet::new())]),
        &BTreeMap::new(),
        &BTreeMap::from([(
            ModuleId(7),
            super::RuntimeReaderOwnerRuntimeState {
                source: "var ownerInit = lazyValue(() => {});".to_string(),
                remaining_helpers: BTreeSet::new(),
                written_helpers: BTreeSet::new(),
                uses_lazy_module: false,
                uses_lazy_value: true,
                can_localize_lazy_value: false,
            },
        )]),
    );

    assert!(selected.contains_key(&BindingName::new("ns")));
    assert!(selected.contains_key(&BindingName::new("target")));
    assert!(
        selected
            .get(&BindingName::new("ns"))
            .is_some_and(|migration| migration.moves_namespace_export)
    );
}

#[test]
fn global_owner_rebuild_prefers_symbol_owner_over_conflicting_span_owner() {
    let owner = super::global_runtime_snippet_owner(Some(ModuleId(7)), None, Some(ModuleId(8)));

    assert_eq!(
        owner,
        Some(ModuleId(7)),
        "unique source-local symbols are stronger than lossy span overlaps"
    );
}

#[test]
fn global_owner_rebuild_can_use_unique_source_span_owner() {
    let modules = vec![
        ModuleInput::application(ModuleId(7), "owner", "modules/owner")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(100, 200)),
        ModuleInput::application(ModuleId(8), "other", "modules/other")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(250, 300)),
    ];

    let owner = super::runtime_snippet_source_span_owner(
        &modules,
        1,
        120,
        "function owned() { return 1; }".len(),
        &BTreeSet::new(),
    );

    assert_eq!(owner, Some(ModuleId(7)));
}

#[test]
fn global_owner_rebuild_prefers_specific_containing_source_span_owner() {
    let modules = vec![
        ModuleInput::application(ModuleId(7), "wrapper", "modules/wrapper")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(100, 500)),
        ModuleInput::application(ModuleId(8), "inner", "modules/inner")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(150, 220)),
    ];

    let owner = super::runtime_snippet_source_span_owner(
        &modules,
        1,
        160,
        "function owned() { return 1; }".len(),
        &BTreeSet::new(),
    );

    assert_eq!(owner, Some(ModuleId(8)));
}

#[test]
fn global_owner_rebuild_rejects_ambiguous_or_externalized_source_span_owner() {
    let modules = vec![
        ModuleInput::application(ModuleId(7), "left", "modules/left")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(100, 200)),
        ModuleInput::application(ModuleId(8), "right", "modules/right")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(150, 250)),
        ModuleInput::package(ModuleId(9), "pkg", "node_modules/pkg", "pkg", None)
            .with_source_file(1)
            .with_source_span(SourceSpan::new(300, 400)),
    ];

    assert_eq!(
        super::runtime_snippet_source_span_owner(&modules, 1, 160, 10, &BTreeSet::new()),
        None
    );
    assert_eq!(
        super::runtime_snippet_source_span_owner(
            &modules,
            1,
            320,
            10,
            &BTreeSet::from([ModuleId(9)])
        ),
        None
    );
}

#[test]
fn runtime_helper_closure_excludes_module_owned_transitive_bindings() {
    let keep = "function keep() { return movedReader() + localDep(); }";
    let moved_reader = "function movedReader() { return movedDep(); }";
    let moved_dep = "function movedDep() { return 1; }";
    let local_dep = "function localDep() { return 2; }";
    let source = [keep, moved_reader, moved_dep, local_dep].join("\n");
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source,
        bindings: BTreeMap::from([
            (
                BindingName::new("keep"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("movedReader"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("movedDep"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("localDep"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (BindingName::new("keep"), snippet(keep)),
            (BindingName::new("movedReader"), snippet(moved_reader)),
            (BindingName::new("movedDep"), snippet(moved_dep)),
            (BindingName::new("localDep"), snippet(local_dep)),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };

    let helper_closure = super::close_runtime_helper_source_excluding(
        &prelude,
        &BTreeSet::from([BindingName::new("keep")]),
        None,
        &[],
        &BTreeSet::from([
            BindingName::new("movedReader"),
            BindingName::new("movedDep"),
        ]),
    );

    assert!(helper_closure.source.contains("function keep()"));
    assert!(helper_closure.source.contains("function localDep()"));
    assert!(helper_closure.source.contains("movedReader()"));
    assert!(!helper_closure.source.contains("function movedReader()"));
    assert!(!helper_closure.source.contains("function movedDep()"));
    assert!(
        helper_closure
            .emitted_bindings
            .contains(&BindingName::new("keep"))
    );
    assert!(
        helper_closure
            .emitted_bindings
            .contains(&BindingName::new("localDep"))
    );
    assert!(
        !helper_closure
            .emitted_bindings
            .contains(&BindingName::new("movedReader"))
    );
}

#[test]
fn emit_plan_coalesces_generated_named_exports() {
    let mut file = PlannedFile::new("modules/consumer.ts");
    file.push_source("const keep = 1;");
    file.push_source("export { beta };");
    file.push_source("console.log(keep);");
    file.push_source("export { alpha };");
    file.push_source("export { beta };");

    let mut plan = EmitPlan::default();
    finalize_planned_file(&mut file);
    plan.push_file(file);
    let source = planned_source(&plan, "modules/consumer.ts");

    assert!(source.contains("export { alpha, beta };"));
    assert_eq!(source.matches("export {").count(), 1);
    assert!(!source.contains("export { beta };\nconsole.log"));
    assert!(source.contains("console.log(keep);"));
}

#[test]
fn emit_plan_keeps_reexports_and_alias_exports_separate() {
    let mut file = PlannedFile::new("modules/consumer.ts");
    file.push_source("export { beta };");
    file.push_source("export { alpha as renamed };");
    file.push_source("export { gamma } from './gamma.js';");
    file.push_source("export { alpha };");

    let mut plan = EmitPlan::default();
    finalize_planned_file(&mut file);
    plan.push_file(file);
    let source = planned_source(&plan, "modules/consumer.ts");

    assert!(source.contains("export { alpha, beta };"));
    assert!(source.contains("export { alpha as renamed };"));
    assert!(source.contains("export { gamma } from './gamma.js';"));
    assert_eq!(source.matches("export {").count(), 3);
}

#[test]
fn emit_plan_drops_generated_export_colliding_with_an_alias_export_name() {
    // An esbuild module re-exports an internal `F` under the public name `f`
    // (`export { F as f }`) while also having a module-private local `f`. A
    // generated `export { f, st }` would then duplicate the export NAME `f`,
    // which is invalid ESM (`SyntaxError: Duplicate export of 'f'`). The
    // colliding name must be dropped from the generated statement, leaving the
    // alias export authoritative.
    let mut file = PlannedFile::new("modules/asset.ts");
    file.push_source("export { F as f, Q as g };");
    file.push_source("export { f, st };");

    let mut plan = EmitPlan::default();
    finalize_planned_file(&mut file);
    plan.push_file(file);
    let source = planned_source(&plan, "modules/asset.ts");

    assert!(
        source.contains("export { F as f, Q as g };"),
        "alias export stays authoritative: {source}"
    );
    assert!(
        source.contains("export { st };"),
        "the non-colliding generated export survives: {source}"
    );
    assert_eq!(
        source.matches("export of").count(),
        0,
        "no duplicate-export marker text: {source}"
    );
    // `f` must appear exactly once as an export name — only via the alias.
    assert_eq!(
        source.matches(" as f").count(),
        1,
        "f exported once via alias: {source}"
    );
    assert!(
        !source.contains("export { f, st };") && !source.contains("export { f };"),
        "the colliding bare `f` export must be gone: {source}"
    );
}

#[test]
fn emit_plan_coalesces_only_consecutive_plain_var_declarations() {
    let mut file = PlannedFile::new("modules/runtime/source-1-helpers.ts");
    file.push_source(concat!(
        "var alpha;\n",
        "var beta;\n",
        "var gamma = 1;\n",
        "var delta;\n",
        "// barrier\n",
        "var epsilon;\n",
        "var zeta;\n",
        "function run() {\n",
        "  var localA;\n",
        "  var localB;\n",
        "  var keep = localA;\n",
        "}\n",
        "var eta;\n",
        "var theta;\n",
    ));

    let mut plan = EmitPlan::default();
    finalize_planned_file(&mut file);
    plan.push_file(file);
    let source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(source.contains("var alpha, beta;"));
    assert!(source.contains("var gamma = 1;"));
    assert!(source.contains("var delta;\n// barrier\nvar epsilon, zeta;"));
    assert!(source.contains("var localA, localB;"));
    assert!(source.contains("var keep = localA;"));
    assert!(source.contains("var eta, theta;"));
    assert!(!source.contains("var alpha;\nvar beta;"));
    assert!(!source.contains("var epsilon;\nvar zeta;"));
}

#[test]
fn inline_internal_setter_calls_collapses_single_arg_call() {
    let input = "function foo() { __reverts_set_X(42); }";
    let expected = "function foo() { (X = 42); }";
    assert_eq!(inline_internal_setter_calls(input), expected);
}

#[test]
fn inline_internal_setter_calls_preserves_multi_arg_call() {
    // Multi-argument setter call has different semantics from a
    // comma-folded assignment — the comma expression's value would
    // be the LAST operand, but a setter call returns the FIRST. Stay
    // conservative.
    let input = "__reverts_set_X(a, b);";
    assert_eq!(inline_internal_setter_calls(input), input);
}

#[test]
fn inline_internal_setter_calls_preserves_member_access() {
    // `obj.__reverts_set_X` is not a top-level setter reference;
    // member access must be left alone.
    let input = "obj.__reverts_set_X(v);";
    assert_eq!(inline_internal_setter_calls(input), input);
}

#[test]
fn inline_internal_setter_calls_handles_non_ascii_identifiers() {
    // Greek π (U+03C0) is a two-byte UTF-8 sequence; the rewriter
    // must advance through the whole codepoint to preserve the byte
    // stream. Without this, the first byte (0xCF) would be emitted
    // alone and produce `Ï` instead of `π`.
    let input = "var keymap = { π: 'alt+p' }; __reverts_set_X(1);";
    let expected = "var keymap = { π: 'alt+p' }; (X = 1);";
    assert_eq!(inline_internal_setter_calls(input), expected);
}

#[test]
fn inline_internal_setter_calls_skips_setter_inside_string_literal() {
    // A setter-shaped identifier inside a string literal is not a
    // call expression — `skip_non_code_at` jumps over it.
    let input = r#"var s = "__reverts_set_X(1)"; __reverts_set_X(2);"#;
    let expected = r#"var s = "__reverts_set_X(1)"; (X = 2);"#;
    assert_eq!(inline_internal_setter_calls(input), expected);
}

#[test]
fn identifier_reference_positions_treats_spread_as_read() {
    // `obj.X` is property access and must NOT count as a reference,
    // but `...X` (spread) IS a read of the binding X and must count.
    // The check looks back two bytes to distinguish single `.` from
    // the trailing dot of `...`.
    let source = "function f(a) { return [...X, a]; }";
    let positions: Vec<usize> = super::identifier_reference_positions(source, "X").collect();
    assert_eq!(
        positions.len(),
        1,
        "spread `...X` must register as a single read"
    );
}

#[test]
fn identifier_reference_positions_skips_member_access() {
    let source = "function f(obj) { return obj.X + obj.X.Y; }";
    let positions: Vec<usize> = super::identifier_reference_positions(source, "X").collect();
    assert_eq!(
        positions.len(),
        0,
        "property access `obj.X` must NOT be a binding reference"
    );
}

/// Build an `EnrichedProgram` with the binding-shape solution derived from the
/// def-use graph. Use this in tests where planner output should observe real
/// shapes; existing tests that explicitly construct
/// `BindingShapeSolution::default()` are intentionally shape-agnostic.
fn enriched_with_solved_shapes(input: InputBundle) -> reverts_model::EnrichedProgram {
    let model = ProgramModel::from_input(input);
    let binding_shapes = BindingShapeSolution::from_def_use_graph(model.graph().def_use());
    reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        binding_shapes,
    )
}

#[test]
fn enriched_program_plans_real_source_without_synthetic_declarations() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "src/index.ts",
        Some("export const answer = 42;".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    assert_eq!(
        plan.files[0].body[0].trim_end(),
        "export const answer = 42;"
    );
}

#[test]
fn lowers_one_parameter_commonjs_wrapper_without_fixed_helper_name() {
    let source = "var hFA = U((uO) => { var U = 1; uO.value = U; });\nhFA();";
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(lowered.source.contains("var hFA = (() => {"));
    assert!(lowered.source.contains("let _$cached;"));
    assert!(lowered.source.contains("var U = 1; uO.value = U;"));
    assert!(!lowered.source.contains("hFA = U("));
    assert!(!lowered.uses_lazy_module);
    assert!(lowered.lowered_helpers.contains(&BindingName::new("U")));
}

#[test]
fn end_to_end_lowering_chain_recovers_idiomatic_esm_from_real_bundle_shape() {
    // A realistic esbuild-style bundle that exercises every recovery
    // pass in one go:
    //   - `require_const` — single-value CJS `module.exports = 42` →
    //     Phase 6a collapses to a direct number binding.
    //   - `require_config` — multi-primitive `exports.K = primitive` →
    //     Phase 6b collapses to a grouped object literal (config record).
    //   - `require_api` — multi-function `exports.K = function/arrow` →
    //     Phase 6b collapses to an object literal, then Phase 7 explodes
    //     it into three top-level function bindings (API namespace).
    //   - `get_palette` — pure `__lazy(() => { return { ... }; })` →
    //     Phase 5 collapses to a direct object binding.
    let source = concat!(
        "var require_const = $wrap((exports, module) => { module.exports = 42; });\n",
        "var require_config = $wrap((exports, module) => { exports.port = 8080; exports.host = \"localhost\"; });\n",
        "var require_api = $wrap((exports, module) => { exports.parse = function(s) { return JSON.parse(s); }; exports.stringify = function(o) { return JSON.stringify(o); }; exports.identity = function(x) { return x; }; });\n",
        "var get_palette = $lazy(() => { return { primary: \"#abc\", secondary: \"#def\" }; });\n",
        "console.log(\"const:\", require_const());\n",
        "console.log(\"port:\", require_config().port);\n",
        "console.log(\"host:\", require_config().host);\n",
        "console.log(\"parsed:\", require_api().parse('{\"x\":1}').x);\n",
        "console.log(\"stringified:\", require_api().stringify({ ok: true }));\n",
        "console.log(\"identity:\", require_api().identity(99));\n",
        "console.log(\"primary:\", get_palette().primary);\n",
        "console.log(\"secondary:\", get_palette().secondary);\n",
    );
    let helper_kinds = BTreeMap::from([
        (
            BindingName::new("$wrap"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        ),
        (
            BindingName::new("$lazy"),
            RuntimePreludeBindingKind::LazyInitializer,
        ),
    ]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // Phase 6a: pure-value module.exports collapsed.
    assert!(lowered.source.contains("var require_const = 42;"));
    // Phase 6b: primitive-valued namespace stays grouped.
    assert!(
        lowered
            .source
            .contains("var require_config = { port: 8080, host: \"localhost\" };")
    );
    // Phase 7: function-valued namespace decomposed into bare bindings.
    assert!(!lowered.source.contains("var require_api"));
    assert!(
        lowered
            .source
            .contains("var parse = function(s) { return JSON.parse(s); };")
    );
    assert!(
        lowered
            .source
            .contains("var stringify = function(o) { return JSON.stringify(o); };")
    );
    assert!(
        lowered
            .source
            .contains("var identity = function(x) { return x; };")
    );
    // Phase 5: lazy value with pure object body collapsed.
    assert!(
        lowered
            .source
            .contains("var get_palette = { primary: \"#abc\", secondary: \"#def\" };")
    );

    // Every consumer call site dropped the dead `()` invocation, attaching
    // member access (or argument lists) directly to the bare identifier.
    assert!(
        lowered
            .source
            .contains("console.log(\"const:\", require_const);")
    );
    assert!(
        lowered
            .source
            .contains("console.log(\"port:\", require_config.port);")
    );
    assert!(
        lowered
            .source
            .contains("console.log(\"parsed:\", parse('{\"x\":1}').x);")
    );
    assert!(
        lowered
            .source
            .contains("console.log(\"stringified:\", stringify({ ok: true }));")
    );
    assert!(
        lowered
            .source
            .contains("console.log(\"identity:\", identity(99));")
    );
    assert!(
        lowered
            .source
            .contains("console.log(\"primary:\", get_palette.primary);")
    );

    // No synthetic scaffolding survives in the lowered source.
    assert!(!lowered.source.contains("lazyModule("));
    assert!(!lowered.source.contains("= lazyValue("));
    assert!(!lowered.source.contains("_$"));
    assert!(!lowered.source.contains("__reverts_"));
    // Every reshape was tracked so the planner can downgrade the
    // IR-inferred shape before the audit checks declaration callability.
    for name in [
        "require_const",
        "require_config",
        "get_palette",
        "parse",
        "stringify",
        "identity",
    ] {
        assert!(
            lowered.reshaped_bindings.contains(&BindingName::new(name)),
            "reshaped_bindings missing {name}; got {:?}",
            lowered.reshaped_bindings
        );
    }
}

#[test]
fn delazify_collapses_pure_object_literal_lazy_value() {
    let source = concat!(
        "var palette = L(() => { return { primary: '#abc', secondary: '#def' }; });\n",
        "console.log(palette().primary);\n",
        "use(palette().secondary);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // Declaration collapsed back to a direct value, callers stripped of `()`.
    assert!(
        lowered
            .source
            .contains("var palette = { primary: '#abc', secondary: '#def' };")
    );
    assert!(lowered.source.contains("console.log(palette.primary);"));
    assert!(lowered.source.contains("use(palette.secondary);"));
    assert!(!lowered.source.contains("= lazyValue("));
    assert!(!lowered.uses_lazy_value);
}

#[test]
fn delazify_skips_lazy_value_used_as_first_class_function() {
    let source = concat!(
        "var thunk = L(() => { return 42; });\n",
        // `thunk` captured as value — not safe to inline because the
        // resulting binding has a different identity (a value vs a
        // function returning the value).
        "register(thunk);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(lowered.source.contains("var _$l"));
    assert!(lowered.source.contains("var thunk = _$l(() => {"));
    assert!(lowered.source.contains("return 42;"));
    assert!(lowered.source.contains("register(thunk);"));
    assert!(!lowered.source.contains("= lazyValue("));
    assert!(!lowered.uses_lazy_value);
}

#[test]
fn delazify_skips_exported_lazy_value() {
    let source = concat!(
        "var settings = L(() => { return { ready: true }; });\n",
        "if (settings().ready) {}\n",
        "export { settings };\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // `export { settings };` references `settings` as a value, not a call —
    // delazifying would change consumer semantics across the module
    // boundary. Keep the lazy thunk shape, but inline the tiny memoizer
    // locally instead of importing the shared runtime helper.
    assert!(lowered.source.contains("var _$l"));
    assert!(lowered.source.contains("var settings = _$l(() => {"));
    assert!(lowered.source.contains("return { ready: true };"));
    assert!(lowered.source.contains("export { settings };"));
    assert!(!lowered.source.contains("= lazyValue("));
    assert!(!lowered.uses_lazy_value);
}

#[test]
fn delazify_skips_lazy_value_with_side_effect_body() {
    let source = concat!("var init = L(() => { setup(); });\n", "init();\n",);
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // Body has a side-effect call but no `return EXPR;` — collapsing to
    // `var init = ...` would evaluate the side effect at module load.
    // Keep the lazy thunk semantics via a local memoizer.
    assert!(lowered.source.contains("var _$l"));
    assert!(lowered.source.contains("var init = _$l(() => {"));
    assert!(lowered.source.contains("setup();"));
    assert!(!lowered.source.contains("= lazyValue("));
    assert!(!lowered.uses_lazy_value);
}

#[test]
fn delazify_skips_lazy_value_with_impure_returned_expression() {
    let source = concat!(
        "var lazy = L(() => { return loadConfig(); });\n",
        "use(lazy());\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // `loadConfig()` is a function call — could have side effects or
    // depend on later state. Don't change evaluation timing.
    assert!(lowered.source.contains("var _$l"));
    assert!(lowered.source.contains("var lazy = _$l(() => {"));
    assert!(lowered.source.contains("return loadConfig();"));
    assert!(!lowered.source.contains("= lazyValue("));
    assert!(!lowered.uses_lazy_value);
}

#[test]
fn inline_remaining_lazy_value_skips_assignment_factories() {
    let source = concat!(
        "var target;\n",
        "var init = L(() => { target ||= makeValue(); });\n",
        "register(init);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // Assignment bodies are the writer side for runtime-var folding. Keep
    // the canonical lazyValue shape so later phases can still see the
    // assignment and move it with the runtime binding.
    assert!(!lowered.source.contains("var _$l"));
    assert!(lowered.source.contains("var init = lazyValue(() => {"));
    assert!(lowered.source.contains("target ||= makeValue();"));
    assert!(lowered.uses_lazy_value);
}

#[test]
fn final_lazy_value_localization_allows_assignment_factories() {
    let source = concat!(
        "var target;\n",
        "var init = lazyValue(() => { target ||= makeValue(); });\n",
        "register(init);\n",
    );

    let (localized, changed) = inline_remaining_lazy_value_wrappers_allowing_assignments(source);

    assert!(changed);
    assert!(localized.contains("var _$l"));
    assert!(localized.contains("var init = _$l(() => {"));
    assert!(localized.contains("target ||= makeValue();"));
    assert!(!localized.contains("= lazyValue("));
}

#[test]
fn inline_remaining_lazy_value_ignores_assignment_lookalikes() {
    let source = concat!(
        "var init = L(() => {\n",
        "  // text: target = value\n",
        "  if (\"target = value\" === 'target = value') setup();\n",
        "  return () => \"target = value\";\n",
        "});\n",
        "register(init);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(lowered.source.contains("var _$l"));
    assert!(lowered.source.contains("var init = _$l(() => {"));
    assert!(
        lowered
            .source
            .contains("\"target = value\" === 'target = value'")
    );
    assert!(lowered.source.contains("return () => \"target = value\";"));
    assert!(!lowered.source.contains("= lazyValue("));
    assert!(!lowered.uses_lazy_value);
}

#[test]
fn inline_remaining_lazy_value_uses_collision_free_helper_name() {
    let source = concat!(
        "var _$l = 'user binding';\n",
        "var thunk = L(() => { setup(); });\n",
        "register(thunk);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(
        lowered.source.contains("var _$l1"),
        "expected synthesized helper to avoid the existing user binding:\n{}",
        lowered.source
    );
    assert!(lowered.source.contains("var _$l = 'user binding';"));
    assert!(lowered.source.contains("var thunk = _$l1(() => {"));
    assert!(!lowered.source.contains("= lazyValue("));
    assert!(!lowered.uses_lazy_value);
}

#[test]
fn inline_remaining_lazy_value_keeps_shared_helper_when_runtime_import_remains() {
    let source = concat!(
        "runtimeDep();\n",
        "var thunk = L(() => { setup(); });\n",
        "register(thunk);\n",
    );
    let helper_kinds = BTreeMap::from([
        (
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        ),
        (
            BindingName::new("runtimeDep"),
            RuntimePreludeBindingKind::SourceBacked,
        ),
    ]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(!lowered.source.contains("var _$l"));
    assert!(lowered.source.contains("var thunk = lazyValue(() => {"));
    assert!(lowered.source.contains("runtimeDep();"));
    assert!(lowered.uses_lazy_value);
    assert!(
        lowered
            .remaining_helpers
            .contains(&BindingName::new("runtimeDep")),
        "source-backed runtime binding should still be imported"
    );
}

#[test]
fn delazify_skips_lazy_value_called_with_arguments() {
    let source = concat!(
        "var thunk = L(() => { return [1, 2, 3]; });\n",
        // Calling the thunk with an argument is meaningless under lazy
        // semantics (the factory takes no args), but our delazify pass
        // shouldn't touch it — the binding shape is wrong for inlining.
        "thunk(0);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(lowered.source.contains("var _$l"));
    assert!(lowered.source.contains("var thunk = _$l(() => {"));
    assert!(lowered.source.contains("thunk(0);"));
    assert!(!lowered.source.contains("= lazyValue("));
    assert!(!lowered.uses_lazy_value);
}

#[test]
fn delazify_preserves_member_access_after_collapsed_call_site() {
    let source = concat!(
        "var theme = L(() => { return { color: 'red' }; });\n",
        "render(theme().color, theme()['palette']);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // After delazify both `theme()` call sites collapse and member access
    // continues to work naturally — `.color` and `['palette']` attach to
    // the bare identifier.
    assert!(lowered.source.contains("var theme = { color: 'red' };"));
    assert!(
        lowered
            .source
            .contains("render(theme.color, theme['palette']);")
    );
    assert!(!lowered.uses_lazy_value);
}

#[test]
fn delazify_collapses_function_expression_value() {
    let source = concat!(
        "var handler = L(() => { return function(req) { return req.path; }; });\n",
        "register(handler());\n",
        // handler() returns a function; consumers do `handler()(arg)` to
        // call the returned function. After delazify: `handler` IS the
        // function, `handler(arg)` calls it directly.
        "var result = handler()(req);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(
        lowered
            .source
            .contains("var handler = function(req) { return req.path; };")
    );
    // `register(handler())` collapses to `register(handler)` because the
    // consumer was using the lazy-returned function as a value.
    assert!(lowered.source.contains("register(handler);"));
    // `handler()(req)` collapses to `handler(req)` — the chained call
    // attaches naturally to the now-bare identifier.
    assert!(lowered.source.contains("var result = handler(req);"));
    assert!(!lowered.uses_lazy_value);
}

#[test]
fn delazify_collapses_lazy_module_with_single_value_export() {
    let source = concat!(
        "var entry = U((exports, module) => { module.exports = 42; });\n",
        "console.log(entry());\n",
        "use(entry());\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(lowered.source.contains("var entry = 42;"));
    assert!(lowered.source.contains("console.log(entry);"));
    assert!(lowered.source.contains("use(entry);"));
    assert!(!lowered.source.contains("lazyModule("));
    assert!(!lowered.uses_lazy_module);
}

#[test]
fn delazify_collapses_lazy_module_with_object_literal_export() {
    let source = concat!(
        "var config = U((exports, module) => { module.exports = { port: 8080, host: 'localhost' }; });\n",
        "listen(config().port, config().host);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(
        lowered
            .source
            .contains("var config = { port: 8080, host: 'localhost' };")
    );
    assert!(lowered.source.contains("listen(config.port, config.host);"));
    assert!(!lowered.uses_lazy_module);
}

#[test]
fn delazify_collapses_lazy_module_with_class_export() {
    let source = concat!(
        "var Foo = U((exports, module) => { module.exports = class Foo { constructor() {} }; });\n",
        "new (Foo())();\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(
        lowered
            .source
            .contains("var Foo = class Foo { constructor() {} };")
    );
    assert!(lowered.source.contains("new (Foo)();"));
    assert!(!lowered.uses_lazy_module);
}

#[test]
fn delazify_collapses_multi_declarator_lazy_modules_in_one_statement() {
    // esbuild co-declares several lazy-module handles in ONE `var` statement
    // (`var a=U(...),b=U(...)`). Each must lower like a single-init statement
    // so BOTH handle names become real, exportable bindings — otherwise the
    // co-declared handles are emitted by nobody and consumers' `b()` dangle.
    let source = concat!(
        "var a = U((exports, module) => { module.exports = 1; }), b = U((exports, module) => { module.exports = 2; });\n",
        "use(a(), b());\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(
        lowered.source.contains("var a = 1") && lowered.source.contains("b = 2"),
        "both co-declared handles must lower to values: {}",
        lowered.source
    );
    assert!(
        lowered.source.contains("use(a, b)"),
        "both call sites collapse: {}",
        lowered.source
    );
    assert!(
        !lowered.uses_lazy_module,
        "no residual thunk: {}",
        lowered.source
    );
}

#[test]
fn delazify_collapses_three_declarator_lazy_modules_in_one_statement() {
    // The declarator loop must handle an arbitrary-length comma chain, not
    // just a pair — every co-declared handle becomes its own value binding.
    let source = concat!(
        "var a = U((exports, module) => { module.exports = 1; }), \
b = U((exports, module) => { module.exports = 2; }), \
c = U((exports, module) => { module.exports = 3; });\n",
        "use(a(), b(), c());\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(
        lowered.source.contains("var a = 1")
            && lowered.source.contains("b = 2")
            && lowered.source.contains("c = 3"),
        "all three co-declared handles must lower to values: {}",
        lowered.source
    );
    assert!(
        lowered.source.contains("use(a, b, c)"),
        "all three call sites collapse: {}",
        lowered.source
    );
    assert!(
        !lowered.uses_lazy_module,
        "no residual thunk: {}",
        lowered.source
    );
}

#[test]
fn helper_rename_leaves_mixed_declarator_statement_untouched() {
    // Conservative bail: if a multi-declarator statement mixes a helper call
    // with a non-helper declarator, lowering only the helper one would sever
    // the comma list into malformed JS (`var a = lazyModule(...);, c = 5;`).
    // Decline the whole statement instead — esbuild never emits this shape,
    // and leaving it intact keeps the output valid.
    let source = "var a = U((exports, module) => { module.exports = 1; }), c = 5;\nuse(a(), c);\n";
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert_eq!(
        lowered.source, source,
        "mixed declarator statement must be left exactly as-is: {}",
        lowered.source
    );
    assert!(
        !lowered.source.contains(";,") && !lowered.source.contains("lazyModule"),
        "no malformed half-lowering: {}",
        lowered.source
    );
}

#[test]
fn delazify_collapses_lazy_module_with_multiple_exports_assignments() {
    let source = concat!(
        "var api = U((exports, module) => { exports.foo = 1; exports.bar = 2; });\n",
        "use(api().foo, api().bar);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // Multi-property `exports.foo = ...; exports.bar = ...;` collapses
    // back to an inline object literal — same observable surface to
    // consumers (member access on the binding) without the lazy thunk.
    assert!(lowered.source.contains("var api = { foo: 1, bar: 2 };"));
    assert!(lowered.source.contains("use(api.foo, api.bar);"));
    assert!(!lowered.uses_lazy_module);
}

#[test]
fn delazify_collapses_lazy_module_with_single_param_property_exports() {
    let source = concat!(
        // Single-param form `(exports) =>` with property assignments only —
        // this is the common minified shape when the bundler doesn't use
        // `module`. The collapse uses the exports param name to detect
        // the property targets.
        "var bag = U((uO) => { uO.value = 1; uO.flag = true; });\n",
        "use(bag().value);\n",
        "check(bag().flag);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(
        lowered
            .source
            .contains("var bag = { value: 1, flag: true };")
    );
    assert!(lowered.source.contains("use(bag.value);"));
    assert!(lowered.source.contains("check(bag.flag);"));
    assert!(!lowered.uses_lazy_module);
}

#[test]
fn delazify_skips_lazy_module_with_mixed_property_assignment_and_statement() {
    let source = concat!(
        "var bag = U((exports, module) => { var helper = 1; exports.foo = helper; });\n",
        "use(bag().foo);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // The local `var helper = 1` would have to be hoisted to the
    // consumer or inlined; the current pass refuses anything other
    // than a pure series of `exports.K = PURE_EXPR;` statements.
    assert!(lowered.source.contains("var bag = (() => {"));
    assert!(!lowered.source.contains("lazyModule("));
    assert!(!lowered.uses_lazy_module);
}

#[test]
fn delazify_skips_lazy_module_with_bracket_indexed_exports() {
    let source = concat!(
        // `exports['default']` is computed-key access; out of scope —
        // we only recover bare-identifier keys.
        "var api = U((exports, module) => { exports['default'] = 1; });\n",
        "use(api().default);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(lowered.source.contains("var api = (() => {"));
    assert!(!lowered.source.contains("lazyModule("));
    assert!(!lowered.uses_lazy_module);
}

#[test]
fn delazify_skips_lazy_module_when_body_does_function_call() {
    let source = concat!(
        "var cached = U((exports, module) => { module.exports = computeHeavy(); });\n",
        "use(cached());\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // `computeHeavy()` is impure — collapsing would change evaluation
    // timing (module load vs. first access). Keep the lazy wrapper.
    assert!(lowered.source.contains("var cached = (() => {"));
    assert!(!lowered.source.contains("lazyModule("));
    assert!(!lowered.uses_lazy_module);
}

#[test]
fn delazify_collapses_lazy_module_with_single_property_single_param() {
    let source = concat!(
        "var bag = U((uO) => { uO.value = 1; });\n",
        "use(bag().value);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // Single-param `(exports) =>` with a single `exports.key = pure;`
    // statement is the simplest property-export shape — collapses to
    // an inline single-key object literal.
    assert!(lowered.source.contains("var bag = { value: 1 };"));
    assert!(lowered.source.contains("use(bag.value);"));
    assert!(!lowered.uses_lazy_module);
}

#[test]
fn delazify_skips_exported_lazy_module() {
    let source = concat!(
        "var sharedConst = U((exports, module) => { module.exports = 100; });\n",
        "console.log(sharedConst());\n",
        "export { sharedConst };\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // Exported binding — cross-module callers would still do
    // `sharedConst()` after we inline. Until the named-import rewriter
    // lands, keep the lazy wrap so the export shape stays a function.
    assert!(lowered.source.contains("var sharedConst = (() => {"));
    assert!(!lowered.source.contains("lazyModule("));
    assert!(!lowered.uses_lazy_module);
}

#[test]
fn delazify_mixes_lazy_value_and_lazy_module_in_one_pass() {
    let source = concat!(
        "var port = L(() => { return 3000; });\n",
        "var host = U((exports, module) => { module.exports = 'localhost'; });\n",
        "listen(host(), port());\n",
    );
    let helper_kinds = BTreeMap::from([
        (
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        ),
        (
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        ),
    ]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(lowered.source.contains("var port = 3000;"));
    assert!(lowered.source.contains("var host = 'localhost';"));
    assert!(lowered.source.contains("listen(host, port);"));
    assert!(!lowered.uses_lazy_value);
    assert!(!lowered.uses_lazy_module);
}

#[test]
fn decompose_function_namespace_explodes_api_object_into_top_level_bindings() {
    // Bundle-shape input: consumers call the thunk `api()` then access
    // the property. The intermediate Phase 6b object literal explodes
    // back into two top-level bindings whose call sites lose the
    // namespace prefix.
    let source = concat!(
        "var api = U((exports, module) => { exports.parse = function(s) { return s; }; ",
        "exports.stringify = function(o) { return o; }; });\n",
        "api().parse('x');\n",
        "api().stringify({});\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(
        !lowered.source.contains("var api ="),
        "got: {}",
        lowered.source
    );
    assert!(
        lowered
            .source
            .contains("var parse = function(s) { return s; };")
    );
    assert!(
        lowered
            .source
            .contains("var stringify = function(o) { return o; };")
    );
    // Call sites: bundle's `api().parse(x)` → `api.parse(x)` after Phase
    // 6b's `()` drop → `parse(x)` after Phase 7's namespace decomposition.
    assert!(lowered.source.contains("parse('x');"));
    assert!(lowered.source.contains("stringify({});"));
    assert!(!lowered.source.contains("api."));
}

#[test]
fn decompose_function_namespace_keeps_primitive_record_grouped() {
    let source = concat!(
        "var config = U((exports, module) => { exports.port = 8080; exports.host = 'localhost'; });\n",
        "listen(config().port, config().host);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // Primitive values describe a data record (config-like). The object
    // stays grouped — splitting would scatter "port" / "host" / "name"
    // / "value" sort of generic identifiers into the module scope and
    // risk both readability loss and collisions.
    assert!(
        lowered
            .source
            .contains("var config = { port: 8080, host: 'localhost' };")
    );
    assert!(lowered.source.contains("listen(config.port, config.host);"));
}

#[test]
fn decompose_function_namespace_keeps_mixed_function_and_primitive_grouped() {
    let source = concat!(
        "var bag = U((exports, module) => { exports.fn = function() { return 1; }; exports.flag = true; });\n",
        "bag().fn();\n",
        "check(bag().flag);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // Mixed function + primitive object — splitting would only partially
    // restore "named exports" semantics and is harder for a reader to
    // reason about. Conservative: keep grouped.
    assert!(lowered.source.contains("var bag = {"));
    assert!(lowered.source.contains("fn: function()"));
    assert!(lowered.source.contains("flag: true"));
    assert!(lowered.source.contains("bag.fn();"));
    assert!(lowered.source.contains("check(bag.flag);"));
}

#[test]
fn decompose_function_namespace_skips_when_binding_passed_as_value() {
    // Mixed access pattern: some `api().method()` (member call), some
    // `api()` alone treated as a value passed to `register`. The bare
    // `api()` collapse to `api` (Phase 6b) makes `register(api)` a
    // namespace handoff — decomposing would break the consumer.
    let source = concat!(
        "var api = U((exports, module) => { exports.run = function() {}; exports.stop = function() {}; });\n",
        "register(api());\n",
        "api().run();\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(lowered.source.contains("var api = {"));
    assert!(lowered.source.contains("register(api);"));
}

#[test]
fn decompose_function_namespace_skips_when_exported() {
    let source = concat!(
        "var api = U((exports, module) => { exports.parse = function(s) { return s; }; exports.stringify = function(o) { return o; }; });\n",
        "api().parse('x');\n",
        "export { api };\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // `export { api };` references the binding by value — cross-module
    // consumers see `api`, not `parse` / `stringify` individually. The
    // upstream Phase 6b refuses to inline the lazy thunk in this case,
    // so Phase 7 has nothing to decompose either.
    assert!(lowered.source.contains("var api = (() => {"));
    assert!(!lowered.source.contains("lazyModule("));
    assert!(lowered.source.contains("export { api };"));
}

#[test]
fn decompose_function_namespace_skips_when_key_collides_with_existing_binding() {
    let source = concat!(
        "var parse = 'pre-existing';\n",
        "var api = U((exports, module) => { exports.parse = function(s) { return s; }; exports.stringify = function(o) { return o; }; });\n",
        "api().parse('x');\n",
        "api().stringify({});\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // The top-level `var parse = 'pre-existing'` would conflict with the
    // decomposed `var parse = function(s)`. Skip the decomposition; the
    // user must rename one of them by hand if they want the cleaner form.
    assert!(lowered.source.contains("var api = {"));
    assert!(lowered.source.contains("api.parse"));
    assert!(lowered.source.contains("api.stringify"));
    assert!(lowered.source.contains("var parse = 'pre-existing';"));
}

#[test]
fn decompose_function_namespace_skips_unknown_key_access() {
    let source = concat!(
        "var api = U((exports, module) => { exports.parse = function() {}; exports.stringify = function() {}; });\n",
        // `api.unknown` is not a key in the object — could be a typo
        // bug in the bundle, or a dynamically-added property elsewhere.
        // Decomposing would silently lose the access; keep grouped.
        "api().unknown();\n",
        "api().parse();\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(lowered.source.contains("var api = {"));
    assert!(lowered.source.contains("api.unknown()"));
}

#[test]
fn decompose_function_namespace_explodes_class_values() {
    let source = concat!(
        "var lib = U((exports, module) => { ",
        "exports.Service = class Service { constructor() {} }; ",
        "exports.Worker = class Worker { run() {} }; ",
        "});\n",
        "new (lib().Service)();\n",
        "new (lib().Worker)();\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // Class expressions count as "function-shape" values for the namespace
    // decomposition — they're API surfaces, not data.
    assert!(!lowered.source.contains("var lib ="));
    assert!(
        lowered
            .source
            .contains("var Service = class Service { constructor() {} };")
    );
    assert!(
        lowered
            .source
            .contains("var Worker = class Worker { run() {} };")
    );
    assert!(lowered.source.contains("new (Service)();"));
    assert!(lowered.source.contains("new (Worker)();"));
}

#[test]
fn decompose_function_namespace_explodes_arrow_function_values() {
    let source = concat!(
        "var fns = U((exports, module) => { exports.add = (a, b) => a + b; exports.sub = (a, b) => a - b; });\n",
        "fns().add(1, 2);\n",
        "fns().sub(3, 4);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    // Arrow function expressions also count.
    assert!(!lowered.source.contains("var fns ="));
    assert!(lowered.source.contains("var add = (a, b) => a + b;"));
    assert!(lowered.source.contains("var sub = (a, b) => a - b;"));
    assert!(lowered.source.contains("add(1, 2);"));
    assert!(lowered.source.contains("sub(3, 4);"));
}

#[test]
fn decompose_function_namespace_skips_when_no_access_sites() {
    // Bundle source has the lazyModule definition but the consumer never
    // calls `unused()`. Phase 6b still inlines the literal (the "all uses
    // are X()" check is vacuously satisfied when there are zero uses),
    // but Phase 7 then has nothing to decompose — decomposing into bare
    // `var parse = ...; var stringify = ...;` would inject unreferenced
    // top-level bindings that look authored. Keep the grouped form so
    // the dead code stays visibly grouped under its original name.
    let source = concat!(
        "var unused = U((exports, module) => { exports.parse = function() {}; exports.stringify = function() {}; });\n",
        "other();\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("U"),
        RuntimePreludeBindingKind::CommonJsWrapper,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(lowered.source.contains("var unused = {"));
    assert!(lowered.source.contains("parse: function()"));
    assert!(lowered.source.contains("stringify: function()"));
    // Not decomposed:
    assert!(!lowered.source.contains("var parse ="));
    assert!(!lowered.source.contains("var stringify ="));
}

#[test]
fn delazify_ignores_binding_reference_inside_string_or_comment() {
    let source = concat!(
        "var palette = L(() => { return { primary: '#abc' }; });\n",
        // String literal that mentions the binding name as text — must
        // not be flagged as a "value reference" of the binding.
        "log('palette is great');\n",
        // Comment that mentions the binding name — same.
        "// palette is also documented here\n",
        "use(palette().primary);\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("L"),
        RuntimePreludeBindingKind::LazyInitializer,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert!(
        lowered
            .source
            .contains("var palette = { primary: '#abc' };")
    );
    assert!(lowered.source.contains("use(palette.primary);"));
    // The string content and the comment remain verbatim.
    assert!(lowered.source.contains("'palette is great'"));
    assert!(
        lowered
            .source
            .contains("// palette is also documented here")
    );
    assert!(!lowered.uses_lazy_value);
}

#[test]
fn end_to_end_planner_delazifies_pure_lazy_bindings_and_omits_helper_file() {
    let planner = ImportExportPlanner;
    // A bundle whose runtime prelude defines a `__commonJS`-style wrapper
    // (`$w`) and a `__lazy`-style initializer (`$l`). The body uses them
    // around pure values that should fully delazify back to direct
    // bindings — and the helper module should NOT be emitted, since no
    // module ends up importing `lazyModule` / `lazyValue`.
    let prelude = concat!(
        "var $w = (factory, cache) => () => ",
        "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
        "var $l = (init, cache) => () => (init && (cache = init(init = 0)), cache);\n",
    );
    let body = concat!(
        "var config = $w((exports, module) => { module.exports = { port: 8080, host: 'localhost' }; });\n",
        "var palette = $l(() => { return { primary: '#abc' }; });\n",
        "var api = $w((exports, module) => { exports.parse = function(s) { return s; }; exports.stringify = function(o) { return o; }; });\n",
        "console.log(config().port);\n",
        "render(palette().primary);\n",
        "api().parse('x');\n",
        "api().stringify({});\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    // No runtime helper file should be emitted — neither `lazyModule` nor
    // `lazyValue` is referenced after delazify.
    assert!(
        plan.files
            .iter()
            .all(|file| file.path != "modules/runtime/source-1-helpers.ts")
    );
    let entry = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let entry_source = entry.body.join("\n");

    // Every lazy thunk collapsed to a direct binding; no helper imports.
    assert!(!entry_source.contains("lazyModule("));
    assert!(!entry_source.contains("= lazyValue("));
    assert!(!entry_source.contains("from './runtime/"));

    // Primitive-valued namespaces stay grouped — `config` and `palette`
    // are data records, not API surfaces.
    assert!(
        entry_source.contains("var config = {"),
        "got: {entry_source}"
    );
    assert!(entry_source.contains("port: 8080"));
    assert!(entry_source.contains("host: 'localhost'"));
    assert!(entry_source.contains("var palette = { primary: '#abc' };"));

    // Function-valued namespace `api` decomposes back to individual
    // top-level bindings — restoring the `export function parse / stringify`
    // shape the user would have written in ESM. Helper imports for
    // `lazyModule` / `lazyValue` are gone since nothing references them.
    assert!(!entry_source.contains("var api = "), "got: {entry_source}");
    assert!(entry_source.contains("var parse = function(s)"));
    assert!(entry_source.contains("var stringify = function(o)"));

    // Consumer call sites: member access on data records keeps `X.field`;
    // function-namespace access drops the namespace prefix entirely.
    assert!(entry_source.contains("console.log(config.port);"));
    assert!(entry_source.contains("render(palette.primary);"));
    assert!(entry_source.contains("parse('x');"), "got: {entry_source}");
    assert!(
        entry_source.contains("stringify({});"),
        "got: {entry_source}"
    );
    // And the obsolete namespace form must not survive.
    assert!(!entry_source.contains("api.parse"), "got: {entry_source}");
    assert!(
        !entry_source.contains("api.stringify"),
        "got: {entry_source}"
    );
}

#[test]
fn runtime_noop_helper_localizes_call_only_use_and_unblocks_lazy_value() {
    let prelude = concat!(
        "var $l = (init, cache) => () => (init && (cache = init(init = 0)), cache);\n",
        "var initShim = () => {};\n",
    );
    let body = concat!(
        "var init = $l(() => { initShim(); return 'ok'; });\n",
        "init();\n",
        "export { init };\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(!entry_source.contains("source-1-helpers"), "{entry_source}");
    assert!(!entry_source.contains("void 0;"), "{entry_source}");
    assert!(!entry_source.contains("initShim"), "{entry_source}");
    assert!(
        !entry_source.contains("= lazyValue("),
        "local no-op should let the module-local lazyValue rewrite run:\n{entry_source}"
    );
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn runtime_noop_helper_keeps_value_uses_on_shared_runtime_edge() {
    let prelude = "var initShim = () => {};\n";
    let left_body = "var left = initShim;\nexport { left };\n";
    let right_body = "var right = initShim;\nexport { right };\n";
    let source = format!("{prelude}{left_body}{right_body}");
    let left_start = prelude.len() as u32;
    let left_end = left_start + left_body.len() as u32;
    let right_end = source.len() as u32;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "left", "modules/left.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(left_start, left_end)),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "right", "modules/right.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(left_end, right_end)),
    );

    let plan = plan_from_rows(rows);
    let left_source = planned_source(&plan, "modules/left.ts");
    let right_source = planned_source(&plan, "modules/right.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        left_source.contains("import { initShim } from './runtime/source-1-helpers.js';"),
        "{left_source}"
    );
    assert!(
        right_source.contains("import { initShim } from './runtime/source-1-helpers.js';"),
        "{right_source}"
    );
    assert!(!left_source.contains("function initShim() {}"));
    assert!(helper_source.contains("var initShim = () => {};"));
}

#[test]
fn runtime_noop_helper_erases_partial_import_without_dropping_other_runtime_helpers() {
    let prelude = concat!(
        "var dynamicNoop = () => {};\n",
        "function sharedHelper() { return Date.now(); }\n",
    );
    let left_body = "dynamicNoop();\nvar left = sharedHelper();\nexport { left };\n";
    let right_body = "var right = sharedHelper();\nexport { right };\n";
    let source = format!("{prelude}{left_body}{right_body}");
    let left_start = prelude.len() as u32;
    let left_end = left_start + left_body.len() as u32;
    let right_end = source.len() as u32;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "left", "modules/left.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(left_start, left_end)),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "right", "modules/right.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(left_end, right_end)),
    );

    let plan = plan_from_rows(rows);
    let left_source = planned_source(&plan, "modules/left.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        left_source.contains("import { sharedHelper } from './runtime/source-1-helpers.js';"),
        "{left_source}"
    );
    assert!(!left_source.contains("dynamicNoop"), "{left_source}");
    assert!(!left_source.contains("void 0;"), "{left_source}");
    assert!(
        helper_source.contains("function sharedHelper() { return Date.now(); }"),
        "{helper_source}"
    );
    assert!(
        !helper_source.contains("dynamicNoop"),
        "no other consumer should keep the erased no-op in runtime:\n{helper_source}"
    );
}

#[test]
fn runtime_private_noop_initializer_calls_are_erased_inside_helper() {
    let prelude = concat!(
        "var lazyValue = (factory) => { let called = false, value; return () => (called ? value : (called = true, value = factory())); };\n",
        "var cached;\n",
        "var initCached = lazyValue(() => { cached = 42; });\n",
        "function readCached() { initCached(); return cached; }\n",
    );
    let consumer_body = "var value = readCached();\nexport { value };\n";
    let source = format!("{prelude}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(helper_source.contains("cached = 42;"), "{helper_source}");
    assert!(helper_source.contains("return cached;"), "{helper_source}");
    assert!(!helper_source.contains("initCached"), "{helper_source}");
    assert!(helper_source.contains("export { readCached };"));
}

#[test]
fn runtime_public_noop_internal_calls_are_erased_but_export_stays() {
    let prelude = concat!(
        "var publicNoop = () => {};\n",
        "function callPublicNoop() { publicNoop(); return 1; }\n",
    );
    let caller_body = "var value = callPublicNoop();\nexport { value };\n";
    let public_body = "var exportedNoop = publicNoop;\nexport { exportedNoop };\n";
    let source = format!("{prelude}{caller_body}{public_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "caller", "modules/caller.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + caller_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "public", "modules/public.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + caller_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        helper_source.contains("var publicNoop = () => {};"),
        "{helper_source}"
    );
    assert!(helper_source.contains("return 1;"), "{helper_source}");
    assert!(helper_source.contains("export { callPublicNoop, publicNoop };"));
}

#[test]
fn node_builtin_require_runtime_helper_rewrites_to_direct_default_import() {
    let prelude = concat!(
        "import { createRequire as buildRequire } from 'node:module';\n",
        "var totallyCustomRequire = buildRequire(import.meta.url);\n",
    );
    let body = concat!(
        "var fsModule = totallyCustomRequire('fs');\n",
        "var value = fsModule.readFileSync('/tmp/example', 'utf8');\n",
        "export { value };\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(
        entry_source.contains("import node_fs from 'node:fs';"),
        "{entry_source}"
    );
    assert!(
        entry_source.contains("var fsModule = node_fs;"),
        "{entry_source}"
    );
    assert!(
        !entry_source.contains("totallyCustomRequire"),
        "{entry_source}"
    );
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn node_builtin_require_runtime_helper_keeps_dynamic_and_package_requires() {
    let prelude = concat!(
        "import { createRequire as buildRequire } from 'node:module';\n",
        "var runtimeRequire = buildRequire(import.meta.url);\n",
    );
    let body = concat!(
        "var fsModule = runtimeRequire('fs', sideEffect());\n",
        "var packageModule = runtimeRequire('not-a-node-builtin');\n",
        "export { fsModule, packageModule };\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        entry_source.contains("import { runtimeRequire } from './runtime/source-1-helpers.js';"),
        "{entry_source}"
    );
    assert!(entry_source.contains("runtimeRequire('fs', sideEffect())"));
    assert!(entry_source.contains("runtimeRequire('not-a-node-builtin')"));
    assert!(!entry_source.contains("node_fs"));
    assert!(helper_source.contains("var runtimeRequire = buildRequire(import.meta.url);"));
}

#[test]
fn cross_module_eager_safe_analysis_delazifies_exported_thunk_and_rewrites_consumer() {
    let planner = ImportExportPlanner;
    // Two-module fixture:
    //   - source file 1 declares a CJS-wrapped binding `palette` that
    //     `module.exports = { primary: '#abc' }`.
    //   - source file 2 (the consumer) imports `palette` from the
    //     first file and uses it as `palette().primary` inside a
    //     function body (NestedOnly call form).
    // Phase 8 SHOULD recognise:
    //   - the producer is a singleton SCC in the top-level dep graph
    //     (no cycle through it),
    //   - the consumer's only reference is the zero-arg `palette()`
    //     shape,
    //   - the producer's binding is thunk-wrapped (lazyModule),
    // and therefore eagerify the producer to a direct value AND
    // mechanically rewrite the consumer's `palette()` → `palette`.
    let producer_prelude = concat!(
        "var $w = (factory, cache) => () => ",
        "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
    );
    let producer_body =
        "var palette = $w((exports, module) => { module.exports = { primary: '#abc' }; });\n";
    let producer_source = format!("{producer_prelude}{producer_body}");
    let consumer_source = "function render() { return palette().primary; }\nrender();\n";

    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "producer.js",
        Some(producer_source.clone()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "consumer.js",
        Some(consumer_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "producer", "modules/producer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                producer_prelude.len() as u32,
                producer_source.len() as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(2)
            .with_source_span(SourceSpan::new(0, consumer_source.len() as u32)),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });

    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let producer = plan
        .files
        .iter()
        .find(|file| file.path == "modules/producer.ts")
        .expect("producer file should be planned");
    let consumer = plan
        .files
        .iter()
        .find(|file| file.path == "modules/consumer.ts")
        .expect("consumer file should be planned");
    let producer_source_out = producer.body.join("\n");
    let consumer_source_out = consumer.body.join("\n");

    // Producer: `palette` is exported AND was thunk-wrapped, but
    // Phase 8 cleared it via the SCC + call-form analysis →
    // delazify happens. The emitted producer carries a direct
    // value, not a `lazyModule(...)` wrap.
    assert!(
        producer_source_out.contains("var palette = { primary: '#abc' };"),
        "producer:\n{producer_source_out}"
    );
    assert!(
        !producer_source_out.contains("lazyModule("),
        "producer should not retain lazyModule wrap:\n{producer_source_out}"
    );
    assert!(producer_source_out.contains("export { palette };"));

    // Consumer: the cross-module rewrite stripped `palette()` to
    // bare `palette`. `palette.primary` is now property access on
    // the directly-imported value.
    assert!(
        consumer_source_out.contains("return palette.primary;"),
        "consumer:\n{consumer_source_out}"
    );
    assert!(
        !consumer_source_out.contains("palette()"),
        "consumer should not retain the zero-arg call:\n{consumer_source_out}"
    );
    // The consumer's bare reference `render()` at module top-level
    // is unchanged (not imported, no rewrite).
    assert!(consumer_source_out.contains("render();"));
}

#[test]
fn cross_module_eager_safe_analysis_keeps_lazy_when_consumer_uses_thunk_as_value() {
    let planner = ImportExportPlanner;
    // Same shape as above, but the consumer passes the thunk as a
    // value (`register(palette)`) — a use Phase 8's call-form
    // analyzer rejects. The producer must KEEP the lazy thunk:
    // mechanically rewriting `palette` → ... would have no safe
    // form, and even delazifying alone (without the rewrite) would
    // break `register(palette)` semantics.
    let producer_prelude = concat!(
        "var $w = (factory, cache) => () => ",
        "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
    );
    let producer_body =
        "var palette = $w((exports, module) => { module.exports = { primary: '#abc' }; });\n";
    let producer_source = format!("{producer_prelude}{producer_body}");
    // Consumer uses palette as both a value (passed to `register`)
    // AND as a thunk call (`palette().primary`) — the value use
    // alone disqualifies it from eagerification.
    let consumer_source = concat!(
        "register(palette);\n",
        "function render() { return palette().primary; }\n",
    );

    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "producer.js",
        Some(producer_source.clone()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "consumer.js",
        Some(consumer_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "producer", "modules/producer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                producer_prelude.len() as u32,
                producer_source.len() as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(2)
            .with_source_span(SourceSpan::new(0, consumer_source.len() as u32)),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });

    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let producer = plan
        .files
        .iter()
        .find(|file| file.path == "modules/producer.ts")
        .expect("producer file should be planned");
    let producer_source_out = producer.body.join("\n");

    // Producer stays a lazy thunk because of the consumer's
    // disqualifying value-use of `palette`.
    assert!(
        producer_source_out.contains("var palette = (() => {"),
        "producer:\n{producer_source_out}"
    );
}

#[test]
fn end_to_end_planner_keeps_lazy_thunk_when_export_or_side_effect_blocks_delazify() {
    let planner = ImportExportPlanner;
    // Same prelude. This time the bindings either get exported (forcing
    // the lazy thunk to stay so the cross-module surface remains a
    // function) or have side-effect bodies that can't be safely hoisted
    // to module-load time.
    let prelude = concat!(
        "var $w = (factory, cache) => () => ",
        "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
        "var $l = (init, cache) => () => (init && (cache = init(init = 0)), cache);\n",
    );
    let body = concat!(
        "var entry = $w((exports, module) => { module.exports = 1; });\n",
        "var init = $l(() => { entry(); });\n",
        "init();\n",
        "export { entry };\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    let entry = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let entry_source = entry.body.join("\n");

    // `entry` is the exported CommonJS module wrapper — its memoizer is local
    // and it is NOT a `_$esm`-shaped value thunk, so it stays wrapped.
    assert!(entry_source.contains("var entry = (() => {"));
    assert!(!entry_source.contains("lazyModule("));
    // `init`'s body is `entry();` (no top-level return, so the thunk yields
    // `undefined`) and `init` is invoked in this acyclic single module. The
    // global de-lazify post-pass therefore hoists the body to eager module-eval
    // and replaces the thunk with a no-op stub (observationally identical: the
    // stub also returns `undefined`); the now-dead local `_$l` memoizer is
    // dropped. See `delazify_init_chains`.
    assert!(!entry_source.contains("_$l"), "{entry_source}");
    assert!(entry_source.contains("entry();"), "{entry_source}");
    assert!(
        entry_source.contains("function init() {}"),
        "{entry_source}"
    );
    assert!(!entry_source.contains("= lazyValue("));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "all lazy wrappers are now local, so no shared runtime helper is needed"
    );
}

#[test]
fn source_backed_helper_calls_are_not_lowered_by_shape() {
    let source = concat!(
        "var schemaCache = pvH(() => { return { keys: ['name'] }; });\n",
        "schemaCache.value.keys;\n",
    );
    let helper_kinds = BTreeMap::from([(
        BindingName::new("pvH"),
        RuntimePreludeBindingKind::SourceBacked,
    )]);

    let lowered = lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

    assert_eq!(lowered.source, source);
    assert!(lowered.lowered_helpers.is_empty());
    assert!(lowered.remaining_helpers.contains(&BindingName::new("pvH")));
}

#[test]
fn planner_direct_imports_bare_runtime_prelude_namespace_imports() {
    let prelude = "import * as pathNS from 'path';\n";
    let body = "var value = pathNS.join('a', 'b');\nexport { value };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(entry_source.contains("import * as pathNS from 'path';"));
    assert!(entry_source.contains("pathNS.join('a', 'b')"));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "the prelude import is now consumed directly, so no runtime helper file is needed"
    );
}

#[test]
fn planner_direct_imports_bare_runtime_prelude_default_and_named_imports() {
    let prelude = "import proc, { cwd as cwdAlias, default as procAlias } from 'node:process';\n";
    let body = "var value = [proc, cwdAlias(), procAlias];\nexport { value };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(
        entry_source.contains(
            "import proc, { cwd as cwdAlias, default as procAlias } from 'node:process';"
        )
    );
    assert!(entry_source.contains("procAlias"));
    let helper_source = planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts");
    assert!(helper_source.is_none(), "{helper_source:?}");
}

#[test]
fn planner_keeps_relative_runtime_prelude_imports_on_runtime_path() {
    let prelude = "import * as localNS from './local.js';\n";
    let body = "var value = localNS.value;\nexport { value };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(entry_source.contains("import { localNS } from './runtime/source-1-helpers.js';"));
    assert!(helper_source.contains("import * as localNS from './local.js';"));
    assert!(helper_source.contains("export { localNS };"));
}

#[test]
fn planner_inlines_singleton_helper_after_direct_prelude_import() {
    let prelude = "import { join as pathJoin } from 'path';\nfunction helper() { return 1; }\n";
    let body = "var value = pathJoin('a', String(helper()));\nexport { value };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(entry_source.contains("import { join as pathJoin } from 'path';"));
    assert!(
        entry_source.contains("function helper() { return 1; }"),
        "entry:\n{entry_source}"
    );
    assert!(!entry_source.contains("source-1-helpers"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn planner_direct_imports_namespace_prelude_after_singleton_inline() {
    let prelude = "import * as pathNS from 'path';\nfunction helper() { return 1; }\n";
    let body = "var value = pathNS.join('a', String(helper()));\nexport { value };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(
        entry_source.contains("import * as pathNS from 'path';"),
        "{entry_source}"
    );
    assert!(entry_source.contains("function helper() { return 1; }"));
    assert!(!entry_source.contains("source-1-helpers"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn planner_keeps_shared_prelude_import_on_runtime_path_when_direct_would_grow() {
    let prelude = "import { join as pathJoin } from 'path';\nfunction helper() { return 1; }\n";
    let body_a = "var valueA = pathJoin('a', String(helper()));\nexport { valueA };\n";
    let body_b = "var valueB = pathJoin('b', String(helper()));\nexport { valueB };\n";
    let source = format!("{prelude}{body_a}{body_b}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "a", "modules/a.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body_a.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "b", "modules/b.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + body_a.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let a_source = planned_source(&plan, "modules/a.ts");
    let b_source = planned_source(&plan, "modules/b.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(a_source.contains("import { helper, pathJoin } from './runtime/source-1-helpers.js';"));
    assert!(b_source.contains("import { helper, pathJoin } from './runtime/source-1-helpers.js';"));
    assert!(!a_source.contains("from 'path'"));
    assert!(!b_source.contains("from 'path'"));
    assert!(helper_source.contains("import { join as pathJoin } from 'path';"));
    assert!(helper_source.contains("export { helper, pathJoin };"));
}

#[test]
fn planner_direct_imports_profitable_prelude_binding_even_when_runtime_reads_it() {
    let external_default =
        "VeryLongExternalDefaultRuntimeBindingUsedByBothConsumersToJustifyDirectImport";
    let prelude = format!(
        "import {external_default} from 'pkg';\n\
         function sharedHelper() {{ return {external_default}.value; }}\n"
    );
    let body_a =
        format!("var valueA = [{external_default}, sharedHelper()];\nexport {{ valueA }};\n");
    let body_b =
        format!("var valueB = [{external_default}, sharedHelper()];\nexport {{ valueB }};\n");
    let source = format!("{prelude}{body_a}{body_b}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "a", "modules/a.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body_a.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "b", "modules/b.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + body_a.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let a_source = planned_source(&plan, "modules/a.ts");
    let b_source = planned_source(&plan, "modules/b.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    let direct_import = format!("import {external_default} from 'pkg';");
    assert!(a_source.contains(&direct_import), "a:\n{a_source}");
    assert!(b_source.contains(&direct_import), "b:\n{b_source}");
    assert!(a_source.contains("import { sharedHelper } from './runtime/source-1-helpers.js';"));
    assert!(b_source.contains("import { sharedHelper } from './runtime/source-1-helpers.js';"));
    assert!(
        helper_source.contains(&direct_import),
        "runtime still imports the binding for sharedHelper:\n{helper_source}"
    );
    assert!(
        helper_source.contains("export { sharedHelper };"),
        "runtime public surface should no longer re-export the external binding:\n{helper_source}"
    );
    assert!(
        !helper_source.contains(&format!("export {{ {external_default}, sharedHelper }};"))
            && !helper_source.contains(&format!("export {{ sharedHelper, {external_default} }};")),
        "runtime must not keep a public re-export for the external binding:\n{helper_source}"
    );
}

#[test]
fn planner_inlines_singleton_helper_that_reads_direct_prelude_import() {
    let prelude = "import * as pathNS from 'path';\nfunction helper() { return pathNS.sep; }\n";
    let body = "var value = pathNS.join('a', helper());\nexport { value };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(entry_source.contains("import * as pathNS from 'path';"));
    assert!(entry_source.contains("function helper() { return pathNS.sep; }"));
    assert!(!entry_source.contains("source-1-helpers"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn planner_inlines_singleton_helper_that_reads_exported_source_dependency() {
    let prelude = "function helper() { return decorate('ok'); }\n";
    let entry_body = "var value = helper();\nexport { value };\n";
    let helper_body = "function decorate(value) { return `${value}!`; }\nexport { decorate };\n";
    let source = format!("{prelude}{entry_body}{helper_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + entry_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "decorator", "modules/decorator.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + entry_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(
        entry_source.contains("import { decorate } from './decorator.js';"),
        "{entry_source}"
    );
    assert!(entry_source.contains("function helper() { return decorate('ok'); }"));
    assert!(!entry_source.contains("source-1-helpers"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn planner_keeps_singleton_helper_with_unexported_source_dependency_in_runtime() {
    let prelude = "function helper() { return decorate('ok'); }\n";
    let entry_body = "var value = helper();\nexport { value };\n";
    let helper_body = "function decorate(value) { return `${value}!`; }\n";
    let source = format!("{prelude}{entry_body}{helper_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + entry_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "decorator", "modules/decorator.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + entry_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        entry_source.contains("import { helper } from './runtime/source-1-helpers.js';"),
        "{entry_source}"
    );
    assert!(helper_source.contains("import { decorate } from '../decorator.js';"));
    assert!(helper_source.contains("function helper() { return decorate('ok'); }"));
}

#[test]
fn enriched_program_normalizes_source_before_emit_plan() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "src/index.ts",
        Some("export function add(a,b){return a+b}".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    assert!(plan.files[0].body[0].contains("export function add(a, b)"));
    assert!(plan.files[0].body[0].contains("return a + b;"));
}

#[test]
fn enriched_program_plans_real_source_slice_from_bundle_span() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some("export const one = 1;\nexport const two = 2;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "one", "modules/one.ts")
            .with_source_file(1)
            .with_source_span(reverts_input::SourceSpan::new(0, 21)),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "two", "modules/two.ts")
            .with_source_file(1)
            .with_source_span(reverts_input::SourceSpan::new(22, 43)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    assert_eq!(plan.files[0].body[0].trim_end(), "export const one = 1;");
    assert_eq!(plan.files[1].body[0].trim_end(), "export const two = 2;");
}

#[test]
fn compiler_profile_selects_webpack_preservation_decision() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some("__webpack_require__(1);".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let mut compiler_profile = CompilerProfile::default();
    compiler_profile.insert_module(
        ModuleId(1),
        ModuleCompilerProfile::new(
            CompilerKind::Webpack,
            false,
            vec![CompilerEvidence::Identifier(
                "__webpack_require__".to_string(),
            )],
        ),
    );
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    )
    .with_compiler_profile(compiler_profile);

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    assert_eq!(
        plan.files[0].compiler_preservation.strategy,
        SourceCompilerStrategy::WebpackRuntime
    );
    assert_eq!(
        plan.files[0].compiler_preservation.action,
        CompilerPreservationAction::PreserveWebpackRuntime
    );
    assert_eq!(
        plan.files[0].compiler_preservation.evidence,
        vec![CompilerEvidence::Identifier(
            "__webpack_require__".to_string()
        )]
    );
    assert_eq!(plan.files[0].body[0].trim_end(), "__webpack_require__(1);");
}

#[test]
fn compiler_preservation_actions_cover_known_compilers() {
    assert_eq!(
        CompilerPreservationAction::from_compiler(CompilerKind::Webpack),
        CompilerPreservationAction::PreserveWebpackRuntime
    );
    assert_eq!(
        CompilerPreservationAction::from_compiler(CompilerKind::Esbuild),
        CompilerPreservationAction::PreserveEsbuildHelpers
    );
    assert_eq!(
        CompilerPreservationAction::from_compiler(CompilerKind::Rollup),
        CompilerPreservationAction::PreserveRollupFacade
    );
    assert_eq!(
        CompilerPreservationAction::from_compiler(CompilerKind::Babel),
        CompilerPreservationAction::PreserveBabelTranspiledOutput
    );
    assert_eq!(
        CompilerPreservationAction::from_compiler(CompilerKind::Terser),
        CompilerPreservationAction::PreserveTerserMinifiedOutput
    );
    assert_eq!(
        CompilerPreservationAction::from_compiler(CompilerKind::Unknown),
        CompilerPreservationAction::DirectModuleSource
    );
}

#[test]
fn shape_upgrade_above_namespace_object_drops_known_members_on_purpose() {
    // Design contract: once member-access constraints get upgraded by a
    // stronger constraint (here `Call`), the merged shape is no longer
    // `NamespaceObject`, and the previously collected property names are
    // no longer a reliable surface — so the planner drops them. Pinning
    // this so we notice if a future refactor starts attaching members
    // to shapes other than NamespaceObject.
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "src/index.ts",
        Some("function ns() { return 1; }\nconst v = ns.foo;\nns();".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let enriched = enriched_with_solved_shapes(input);

    // Sanity: solver collects `foo` as a member-access property…
    let raw_members = enriched.known_members(ModuleId(1), "ns");
    assert_eq!(
        raw_members
            .iter()
            .map(BindingName::as_str)
            .collect::<Vec<_>>(),
        vec!["foo"],
    );
    // …but the merged shape settles above NamespaceObject because of
    // the `ns()` call.
    let shape = enriched.binding_shape(ModuleId(1), "ns");
    assert!(
        shape > BindingShape::NamespaceObject,
        "expected merged shape above NamespaceObject, got {shape:?}",
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    let ns_binding = plan.files[0]
        .bindings
        .iter()
        .find(|binding| binding.original.as_str() == "ns")
        .expect("ns binding should be planned");
    assert_eq!(ns_binding.shape, shape);
    assert!(
        ns_binding.known_members.is_empty(),
        "known_members must be empty for non-NamespaceObject shapes, got {:?}",
        ns_binding.known_members,
    );
}

#[test]
fn enriched_program_attaches_known_members_to_cross_module_imported_namespaces() {
    // Paper #7 downstream — cross-application-module path. When moduleB
    // imports `ns` from sibling moduleA and accesses `ns.foo`, the
    // planner's `imports_by_module` wiring runs first and previously
    // hard-coded BindingShape::Unknown. Solver-derived shape must reach
    // the planned binding so `known_members` stays consistent regardless
    // of which import path emits it.
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "modules/a.ts",
        Some("export const ns = { foo: 1, bar: 2 };".to_string()),
    ));
    // moduleB references `ns` without an explicit `import` statement —
    // the kind of cross-module bundle layout the planner's
    // `imports_by_module` wiring synthesizes a named import for.
    // (When an explicit `import` is present, the source-imports path
    // handles the binding; line 289 only fires for this implicit form.)
    rows.source_files.push(SourceFileInput::new(
        2,
        "modules/b.ts",
        Some("const a = ns.foo;\nconst b = ns.bar;".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "a", "modules/a.ts").with_source_file(1));
    rows.modules
        .push(ModuleInput::application(ModuleId(2), "b", "modules/b.ts").with_source_file(2));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let enriched = enriched_with_solved_shapes(input);

    assert_eq!(
        enriched.binding_shape(ModuleId(2), "ns"),
        BindingShape::NamespaceObject,
        "solver must see `ns` as a namespace from moduleB's perspective",
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    let module_b_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/b.ts")
        .expect("moduleB plan should exist");
    let ns_binding = module_b_file
        .bindings
        .iter()
        .find(|binding| binding.original.as_str() == "ns")
        .expect("ns binding from cross-module import should be planned");
    assert_eq!(
        ns_binding.shape,
        BindingShape::NamespaceObject,
        "imports_by_module path must pick up solver-derived shape",
    );
    let members: Vec<_> = ns_binding
        .known_members
        .iter()
        .map(BindingName::as_str)
        .collect();
    assert_eq!(members, vec!["bar", "foo"]);
}

#[test]
fn enriched_program_attaches_known_members_to_imported_namespace_bindings() {
    // Paper #7 downstream — source-imports path: `import * as ns from
    // 'pkg'` followed by `ns.foo` / `ns.bar` should reach the planner
    // with shape=NamespaceObject and known_members={bar, foo}. Before
    // this wiring, the source-imports loop hard-coded BindingShape::
    // Unknown so imported namespaces could never carry known_members.
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "src/index.ts",
        Some("import * as ns from 'pkg';\nconst a = ns.foo;\nconst b = ns.bar;".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    rows.package_surfaces.push(PackageSurfaceInput {
        package_name: "pkg".to_string(),
        package_version: Some("1.0.0".to_string()),
        export_specifier: "pkg".to_string(),
        status: PackageAttributionStatus::Accepted,
        evidence: None,
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let enriched = enriched_with_solved_shapes(input);

    assert_eq!(
        enriched.binding_shape(ModuleId(1), "ns"),
        BindingShape::NamespaceObject,
        "solver should classify the imported namespace",
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    let ns_binding = plan.files[0]
        .bindings
        .iter()
        .find(|binding| binding.original.as_str() == "ns")
        .expect("imported ns binding should be planned");
    assert_eq!(ns_binding.shape, BindingShape::NamespaceObject);
    let members: Vec<_> = ns_binding
        .known_members
        .iter()
        .map(BindingName::as_str)
        .collect();
    assert_eq!(members, vec!["bar", "foo"]);
}

#[test]
fn enriched_program_attaches_known_members_to_namespace_object_bindings() {
    // Paper #7 downstream: when the shape solver classifies a definition
    // as `NamespaceObject` from `ns.foo` / `ns.bar` accesses, the planner
    // must thread those property names onto the `PlannedBinding` so the
    // emitter and audit gates can reason about the namespace's surface.
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "src/index.ts",
        Some("const ns = { foo: 1, bar: 2 };\nconst a = ns.foo;\nconst b = ns.bar;".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let enriched = enriched_with_solved_shapes(input);

    // Sanity-check the shape solver before asserting on planner output.
    assert_eq!(
        enriched.binding_shape(ModuleId(1), "ns"),
        BindingShape::NamespaceObject
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    let ns_binding = plan.files[0]
        .bindings
        .iter()
        .find(|binding| binding.original.as_str() == "ns")
        .expect("ns binding should be planned");
    assert_eq!(ns_binding.shape, BindingShape::NamespaceObject);
    let members: Vec<_> = ns_binding
        .known_members
        .iter()
        .map(BindingName::as_str)
        .collect();
    assert_eq!(members, vec!["bar", "foo"]);

    // Non-NamespaceObject bindings must remain memberless to avoid noise.
    let a_binding = plan.files[0]
        .bindings
        .iter()
        .find(|binding| binding.original.as_str() == "a")
        .expect("a binding should be planned");
    assert!(a_binding.known_members.is_empty());
}

#[test]
fn enriched_program_plans_recovered_bindings_with_shapes() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "src/index.ts",
        Some("function factory() { return 42; }".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let enriched = enriched_with_solved_shapes(input);

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    assert_eq!(plan.files[0].bindings.len(), 1);
    assert_eq!(plan.files[0].bindings[0].original.as_str(), "factory");
    assert_eq!(plan.files[0].bindings[0].shape, BindingShape::Callable);
    assert!(plan.files[0].bindings[0].source_backed);
}

#[test]
fn input_symbol_without_ast_definition_is_planned_as_not_source_backed() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.modules.push(ModuleInput::application(
        ModuleId(1),
        "entry",
        "src/index.ts",
    ));
    rows.symbols.push(SymbolInput::new(ModuleId(1), "missing"));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    let missing = plan.files[0]
        .bindings
        .iter()
        .find(|binding| binding.original.as_str() == "missing")
        .expect("input symbol should be planned");
    assert!(!missing.source_backed);
}

#[test]
fn source_backed_symbol_plans_late_readability_rename() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "src/index.ts",
        Some("var $F1 = 1; export { $F1 };".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    rows.symbols
        .push(SymbolInput::new(ModuleId(1), "$F1").with_semantic_name("lodashGlobalObjectInit"));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let mut semantic_names = reverts_model::SemanticNameMap::default();
    semantic_names.insert_binding(ModuleId(1), "$F1", "lodashGlobalObjectInit");
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        semantic_names,
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let binding = plan.files[0]
        .bindings
        .iter()
        .find(|binding| binding.original.as_str() == "$F1")
        .expect("source binding should be planned");

    assert!(binding.source_backed);
    assert_eq!(binding.emitted.as_str(), "lodashGlobalObjectInit");
    assert_eq!(plan.files[0].exports[0].binding.as_str(), "$F1");
    assert_eq!(plan.files[0].readability_renames.len(), 1);
    assert_eq!(
        plan.files[0].readability_renames[0].original.as_str(),
        "$F1"
    );
    assert_eq!(
        plan.files[0].readability_renames[0].renamed.as_str(),
        "lodashGlobalObjectInit"
    );
}

#[test]
fn generated_overlay_symbol_plans_rename_without_synthetic_binding() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "src/index.ts",
        Some("console.log('generated temp appears after lowering');".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    rows.symbols
        .push(SymbolInput::new(ModuleId(1), "_a").with_semantic_name("generatedTemp"));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let mut semantic_names = reverts_model::SemanticNameMap::default();
    semantic_names.insert_binding(ModuleId(1), "_a", "generatedTemp");
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        semantic_names,
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    assert!(
        !plan.files[0]
            .bindings
            .iter()
            .any(|binding| binding.original.as_str() == "_a")
    );
    assert!(
        plan.files[0]
            .readability_renames
            .iter()
            .any(|rename| rename.original.as_str() == "_a"
                && rename.renamed.as_str() == "generatedTemp")
    );
}

#[test]
fn source_backed_import_plans_late_readability_rename() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "src/index.ts",
        Some("import { map as $F1 } from 'lodash'; export { $F1 };".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let mut semantic_names = reverts_model::SemanticNameMap::default();
    semantic_names.insert_binding(ModuleId(1), "$F1", "lodashMap");
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        semantic_names,
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let binding = plan.files[0]
        .bindings
        .iter()
        .find(|binding| binding.original.as_str() == "$F1")
        .expect("source import binding should be planned");

    assert!(binding.source_backed);
    assert_eq!(binding.emitted.as_str(), "lodashMap");
    assert_eq!(plan.files[0].readability_renames.len(), 1);
    assert_eq!(
        plan.files[0].readability_renames[0].original.as_str(),
        "$F1"
    );
    assert_eq!(
        plan.files[0].readability_renames[0].renamed.as_str(),
        "lodashMap"
    );
}

#[test]
fn enriched_program_plans_source_backed_ast_exports() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "src/index.ts",
        Some("const answer = 42; export { answer };".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    assert_eq!(plan.files[0].exports.len(), 1);
    assert_eq!(plan.files[0].exports[0].binding.as_str(), "answer");
    assert!(plan.files[0].exports[0].source_backed);
}

#[test]
fn source_imported_binding_can_back_source_export() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "src/index.ts",
        Some("import { answer } from 'pkg'; export { answer };".to_string()),
    ));
    rows.modules
        .push(ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");

    assert!(
        plan.files[0]
            .bindings
            .iter()
            .any(|binding| { binding.original.as_str() == "answer" && binding.source_backed })
    );
    assert_eq!(plan.files[0].exports[0].binding.as_str(), "answer");
}

#[test]
fn enriched_program_lowers_runtime_helpers_from_arbitrary_binding_names() {
    let planner = ImportExportPlanner;
    let prelude = concat!(
        "var $wrap7 = (factory, cache) => () => ",
        "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
        "var _lazy9 = (init, cache) => () => (init && (cache = init(init = 0)), cache);\n",
    );
    let body = concat!(
        "var entry = $wrap7((exports, module) => { module.exports = 1; });\n",
        "var init = _lazy9(() => { entry(); });\n",
        "init();\n",
        "export { entry };\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let module_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("module file should be planned");

    assert!(
        plan.files
            .iter()
            .all(|file| file.path != "modules/runtime/source-1-prelude.ts")
    );
    let module_source = module_file.body.join("\n");
    assert!(!module_source.contains("$wrap7"));
    assert!(!module_source.contains("_lazy9"));
    assert!(module_source.contains("var entry = (() => {"));
    assert!(!module_source.contains("lazyModule("));
    assert!(!module_source.contains("= lazyValue("));
    // The remaining CommonJS lazy boundary is now local to this module,
    // so its tiny memoization temps live in the recovered module instead
    // of requiring shared runtime lazyModule/lazyValue imports.
    assert!(module_source.contains("_$cached"));
    assert!(module_source.contains("_$module"));
    // `init` (an `_$esm` value thunk whose body has no top-level return, and
    // which is invoked here) is hoisted to eager module-eval and stubbed by the
    // global de-lazify post-pass; its dead local `_$l` memoizer is dropped.
    assert!(!module_source.contains("_$l"), "{module_source}");
    assert!(module_source.contains("entry();"), "{module_source}");
    assert!(
        module_source.contains("function init() {}"),
        "{module_source}"
    );
}

#[test]
fn entrypoint_runtime_uses_shared_helper_module_with_tail_side_effects() {
    let planner = ImportExportPlanner;
    let prelude = "function main() { return cliEntry(); }\n";
    let body = "var cliEntry = () => 'ok';\nvar cliInit = () => {};\n";
    let tail = "cliInit();\nprocess.env.FLAG = 'ok';\nmain();\n";
    let source = format!("{prelude}{body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body.len()) as u32,
            )),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let module_source = planned_source(&plan, "modules/entry.ts");
    let cli_source = planned_source(&plan, "cli.ts");
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");

    assert!(
        plan.files
            .iter()
            .all(|file| file.path != "modules/runtime/source-1-prelude.ts")
    );
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "entrypoint-only direct module dependencies should not keep a runtime helper barrel"
    );
    assert!(module_source.contains("export { cliEntry, cliInit };"));
    assert!(cli_source.contains("#!/usr/bin/env node"));
    assert!(cli_source.contains("import { main } from './modules/entrypoint.js';"));
    assert!(cli_source.contains("await main();"));
    assert!(!cli_source.contains("function main()"));
    assert!(!cli_source.contains("cliInit();"));
    assert!(!cli_source.contains("process.env.FLAG = 'ok';"));
    assert!(!cli_source.contains("source-1-prelude"));
    assert!(entrypoint_source.contains("import { cliEntry, cliInit } from './entry.js';"));
    assert!(entrypoint_source.contains("function main()"));
    assert!(entrypoint_source.contains("return cliEntry();"));
    assert!(entrypoint_source.contains("cliInit();"));
    assert!(entrypoint_source.contains("process.env.FLAG = 'ok';"));
    assert!(entrypoint_source.contains("export { main };"));
}

#[test]
fn entrypoint_runtime_and_module_setters_share_single_helper_state() {
    let planner = ImportExportPlanner;
    let prelude = "var yA;\nfunction main() { initModule(); return yA(); }\n";
    let body = "yA = () => 'linux';\nfunction initModule() {}\nexport { initModule };\n";
    let tail = "main();\n";
    let source = format!("{prelude}{body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body.len()) as u32,
            )),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let cli_file = plan
        .files
        .iter()
        .find(|file| file.path == "cli.ts")
        .expect("cli entrypoint should be planned");
    let entry_source = entry_file.body.join("\n");
    let cli_source = cli_file.body.join("\n");

    assert!(entry_source.contains("var yA;"));
    assert!(entry_source.contains("yA = () => 'linux';"));
    assert!(!entry_source.contains("__reverts_set_yA"));
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");
    assert!(cli_source.contains("import { main } from './modules/entrypoint.js';"));
    assert!(cli_source.contains("await main();"));
    assert!(!cli_source.contains("var yA"));
    assert!(entrypoint_source.contains("initModule"));
    assert!(entrypoint_source.contains("yA"));
    assert!(entrypoint_source.contains("from './entry.js';"));
    assert!(entrypoint_source.contains("function main()"));
    assert!(entrypoint_source.contains("return yA();"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn entrypoint_island_does_not_inline_phantom_of_module_owned_var() {
    // A runtime var written in a *separate* module and read by the entrypoint is
    // migrated to that writer (which owns the real, assigned declaration). The
    // entrypoint island must obtain it by import, never by inlining its own
    // unassigned `var X;` — that local would shadow the owner's assigned copy
    // and read as `undefined` (the React `__toESM` `b5` phantom that crashed
    // `doctor`). The island's snippet closure must exclude module-owned bindings,
    // matching its root-selection which already refuses to seed them.
    let planner = ImportExportPlanner;
    let prelude = "var rt;\n";
    let writer = "function initRt() { rt = { value: () => 'ok' }; }\nexport { initRt };\n";
    let tail = "function main() { initRt(); return rt.value(); }\nmain();\n";
    let source = format!("{prelude}{writer}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer.len()) as u32,
            )),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );
    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");

    // The entrypoint reads the var...
    assert!(
        entrypoint_source.contains("rt.value()"),
        "entrypoint should still read the migrated var",
    );
    // ...but must not re-declare it as an unassigned local phantom...
    assert!(
        !entrypoint_source.contains("var rt;"),
        "entrypoint must not inline an unassigned local of a module-owned var",
    );
    // ...it must import it from the writer module that owns the assignment.
    assert!(
        entrypoint_source.contains("rt") && entrypoint_source.contains("from './writer.js'"),
        "entrypoint must import the migrated var from its owner module",
    );
}

#[test]
fn entrypoint_island_writes_occupied_runtime_var_through_setter_not_import_assignment() {
    // A runtime var (`st`) is kept in the runtime helper because a helper-resident
    // reader (`peekSt`, pulled in by a module) reads it — so the entrypoint island
    // imports it rather than inlining it. But a snippet the island DID inline
    // (`resetSt`, reachable from `main`) WRITES it. Assigning to an ESM import is
    // illegal (`TypeError: Assignment to constant variable` — the real cc-2.1.89
    // `R14` doing `zh1 = sl6, sl6 = []` that aborted every Ink frame). The island
    // must route the write through the helper's `__reverts_set_st` setter, import
    // that setter, and the helper must declare+export it.
    let planner = ImportExportPlanner;
    let prelude = "var st;\nfunction resetSt() { st = []; }\n";
    let module_body = "function api() { return st.length; }\nexport { api };\n";
    let tail = "function main() { resetSt(); return api(); }\nmain();\n";
    let source = format!("{prelude}{module_body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "api", "modules/api.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + module_body.len()) as u32,
            )),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );
    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    // The island's write is rewritten to a setter call, never a raw assignment to
    // the imported binding.
    assert!(
        entrypoint_source.contains("__reverts_set_st("),
        "island must route its write to the imported runtime var through a setter:\n{entrypoint_source}",
    );
    assert!(
        !entrypoint_source.contains("st = []"),
        "island must not assign to an imported binding directly:\n{entrypoint_source}",
    );
    // The setter is imported into the island...
    assert!(
        entrypoint_source.contains("__reverts_set_st")
            && entrypoint_source.contains("source-1-helpers.js"),
        "island must import the setter from the runtime helper:\n{entrypoint_source}",
    );
    // ...and the owner helper declares + still holds the real mutable var.
    assert!(
        helper_source.contains("function __reverts_set_st"),
        "runtime helper must declare the setter:\n{helper_source}",
    );
    assert!(
        helper_source.contains("var st"),
        "runtime helper must still own the mutable var:\n{helper_source}",
    );
}

#[test]
fn entrypoint_runtime_preserves_side_effect_order_before_later_runtime_declarations() {
    let planner = ImportExportPlanner;
    let prelude = concat!(
        "var Constructor;\n",
        "function initializeConstructor() { Constructor = class RuntimeCommand {}; }\n",
    );
    let body = "export const value = 1;\n";
    let tail = concat!(
        "initializeConstructor();\n",
        "var command = new Constructor();\n",
        "function main() { return command; }\n",
        "main();\n",
    );
    let source = format!("{prelude}{body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body.len()) as u32,
            )),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");

    let initialize_index = entrypoint_source
        .find("initializeConstructor();")
        .expect("entrypoint side effect should be emitted");
    let command_index = entrypoint_source
        .find("var command = new Constructor();")
        .expect("later runtime declaration should be emitted");
    assert!(initialize_index < command_index);
    assert!(entrypoint_source.contains("var Constructor;"));
    assert!(entrypoint_source.contains("function initializeConstructor()"));
    assert!(entrypoint_source.contains("function main()"));
    assert!(entrypoint_source.contains("export { main };"));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "entrypoint-only ordered runtime declarations should stay in the island"
    );
}

#[test]
fn entrypoint_runtime_preserves_namespace_export_order_before_tail_side_effects() {
    let planner = ImportExportPlanner;
    let prelude = concat!(
        "function zString() { return 'schema'; }\n",
        "var m = {};\n",
        "M5(m, { string: () => zString });\n",
    );
    let body = "export const value = 1;\n";
    let tail = concat!(
        "initializeSchemas();\n",
        "function initializeSchemas() { if (typeof m.string !== 'function') throw Error('missing zod string'); }\n",
        "function main() { return m.string(); }\n",
        "main();\n",
    );
    let source = format!("{prelude}{body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body.len()) as u32,
            )),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");

    let namespace_index = entrypoint_source
        .find("Object.defineProperties(m")
        .expect("namespace export should be emitted");
    let side_effect_index = entrypoint_source
        .find("initializeSchemas();")
        .expect("entrypoint side effect should be emitted");
    assert!(
        namespace_index < side_effect_index,
        "namespace export must precede tail side effects that read it, got:\n{entrypoint_source}"
    );
    assert!(entrypoint_source.contains("function main()"));
    assert!(entrypoint_source.contains("export { main };"));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "entrypoint-only namespace runtime should stay in the island"
    );
}

#[test]
fn contextual_identifier_as_is_kept_as_runtime_dependency() {
    let planner = ImportExportPlanner;
    let prelude =
        "var as = { command() { return this; } };\nfunction main() { return as.command('run'); }\n";
    let body = "export const value = 1;\n";
    let tail = "main();\n";
    let source = format!("{prelude}{body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body.len()) as u32,
            )),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");

    assert!(entrypoint_source.contains("var as = { command() { return this; } };"));
    assert!(entrypoint_source.contains("function main()"));
    assert!(entrypoint_source.contains("as.command('run')"));
    assert!(entrypoint_source.contains("export { main };"));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "entrypoint-only runtime dependency named `as` should stay in the island"
    );
}

#[test]
fn strip_runtime_snippet_sources_preserves_template_blank_lines() {
    let keep = "function keep() { return `alpha\n\n      IMPORTANT`; }";
    let drop = "function drop() { return 1; }";
    let tail = "function tail() { return keep(); }";
    let source = format!("{keep}\n{drop}\n{tail}");
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.clone(),
        bindings: BTreeMap::from([
            (
                BindingName::new("keep"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("drop"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([(
            BindingName::new("drop"),
            RuntimePreludeSnippet {
                source: drop.to_string(),
                byte_start: (keep.len() + 1) as u32,
                sub_snippets: Vec::new(),
            },
        )]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let stripped = super::runtime_helper_strip::strip_runtime_snippet_sources(
        source.as_str(),
        &prelude,
        &BTreeSet::from([BindingName::new("drop")]),
    );

    assert!(!stripped.contains("function drop()"));
    assert!(
        stripped.contains("`alpha\n\n      IMPORTANT`"),
        "{stripped}"
    );
    assert!(stripped.contains("function tail()"));
}

#[test]
fn migrated_entrypoint_callee_imports_owner_directly() {
    let prelude = "var main;\n";
    let body = "main = () => 'ok';\nexport { main };\n";
    let tail = "main();\n";
    let source = format!("{prelude}{body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body.len()) as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");
    let cli_source = planned_source(&plan, "cli.ts");

    assert!(entry_source.contains("var main;"), "{entry_source}");
    assert!(entry_source.contains("main = () => 'ok';"));
    assert!(entry_source.contains("export { main };"));
    assert!(cli_source.contains("import { main } from './modules/entry.js';"));
    assert!(cli_source.contains("await main();"));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "entrypoint-only migrated callees should not keep a runtime helper barrel"
    );
}

#[test]
fn entrypoint_runtime_imports_adapter_required_package_bindings() {
    let planner = ImportExportPlanner;
    let prelude = "function main() { return packageInit(); }\n";
    let body = "var value = packageInit();\nexport { value };\n";
    let tail = "packageInit();\nmain();\n";
    let source = format!("{prelude}{body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some("function packageInit() { return 1; }".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "package",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        )
        .with_resolved_file(
            "exact-hint:fixture-package@1.0.0:quality=trusted:semantic_path=modules/package.ts",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/package.ts")
        .expect("adapter-required package file should be emitted");
    let cli_source = planned_source(&plan, "cli.ts");
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");

    assert!(cli_source.contains("import { main } from './modules/entrypoint.js';"));
    assert!(entrypoint_source.contains("import { packageInit } from './package.js';"));
    assert!(entrypoint_source.contains("packageInit();"));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "adapter-required package entrypoint dependencies should import their owner directly"
    );
    let package_source = package_file.body.join("\n");
    assert_eq!(package_file.imports.len(), 1);
    assert_eq!(
        package_file.imports[0].resolution.specifier(),
        Some("fixture-package")
    );
    assert!(package_source.contains("function packageInit() { return external_fixture_package; }"));
    assert!(package_source.contains("export { packageInit };"));
    assert!(!package_source.contains("return 1"));
}

#[test]
fn entrypoint_island_erases_unreferenced_externalized_package_init_imports() {
    let prelude = "function main() { packageInit(); return 1; }\n";
    let body = "export const value = 1;\n";
    let tail = "main();\n";
    let source = format!("{prelude}{body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some("function packageInit() { return 1; }".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "package",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        ));

    let plan = plan_from_rows(rows);
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");

    assert!(
        entrypoint_source.contains("function main()"),
        "{entrypoint_source}"
    );
    assert!(
        !entrypoint_source.contains("packageInit"),
        "{entrypoint_source}"
    );
    assert!(
        !entrypoint_source.contains("./package.js"),
        "{entrypoint_source}"
    );
    assert!(
        entrypoint_source.contains("return 1;"),
        "{entrypoint_source}"
    );
}

#[test]
fn adapter_required_commonjs_package_module_uses_external_adapter() {
    let planner = ImportExportPlanner;
    let app_source = "var value = packageInit().answer;\nexport { value };\n";
    let package_source = r#"
        var packageInit = (() => {
            let _$cached;
            return () => {
                if (_$cached) return _$cached.exports;
                var _$module = _$cached = { exports: {} };
                ((exports) => { exports.answer = 42; })(_$module.exports, _$module);
                return _$module.exports;
            };
        })();
        export { packageInit };
    "#;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "package",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        )
        .with_resolved_file(
            "forced-external:export-members:source-equivalent:answer:fixture-package@1.0.0/index.js",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/package.ts")
        .expect("adapter package file should be emitted");
    let source = package_file.body.join("\n");

    assert_eq!(package_file.imports.len(), 1);
    assert_eq!(
        package_file.imports[0].resolution.specifier(),
        Some("fixture-package")
    );
    assert!(source.contains(
        "function packageInit() { return Object.prototype.hasOwnProperty.call(external_fixture_package, \"default\") ? external_fixture_package.default : external_fixture_package; }"
    ));
    assert!(source.contains("export { packageInit };"));
    assert!(!source.contains("_$cached"));
}

#[test]
fn anonymous_bundle_external_attribution_uses_external_adapter() {
    // An esbuild-inlined ANONYMOUS module (kind = Application, no package_name)
    // that the matcher's anonymous-externalize pass attributed to a package must
    // flow through the same drop + external-package-adapter path as a path-named
    // package module: its source is dropped and replaced with a bare import.
    let planner = ImportExportPlanner;
    let app_source = "var value = packageThing().answer;\nexport { value };\n";
    let package_source = r#"
        var packageThing = (() => {
            let _$cached;
            return () => {
                if (_$cached) return _$cached.exports;
                var _$module = _$cached = { exports: {} };
                ((exports) => { exports.answer = 42; })(_$module.exports, _$module);
                return _$module.exports;
            };
        })();
        export { packageThing };
    "#;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "anon.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "anon-pkg", "modules/anon-pkg.ts")
            .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        )
        .with_resolved_file(
            "forced-external:export-members:source-equivalent:packageThing:fixture-package@1.0.0/index.js",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/anon-pkg.ts")
        .expect("adapter file should be emitted for the anonymous external module");
    let source = package_file.body.join("\n");

    assert_eq!(package_file.imports.len(), 1);
    assert_eq!(
        package_file.imports[0].resolution.specifier(),
        Some("fixture-package")
    );
    assert!(source.contains("export { packageThing };"), "{source}");
    assert!(
        !source.contains("_$cached"),
        "original inlined source must be dropped: {source}"
    );
}

#[test]
fn commonjs_external_adapter_exports_only_original_require_binding() {
    let package_source = r#"
        function helper() { return 42; }
        var packageInit = (() => {
            let _$cached;
            return () => {
                if (_$cached) return _$cached.exports;
                var _$module = _$cached = { exports: {} };
                ((exports) => { exports.answer = helper(); })(_$module.exports, _$module);
                return _$module.exports;
            };
        })();
    "#;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        ));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );
    let bindings = BTreeSet::from([BindingName::new("helper"), BindingName::new("packageInit")]);

    let narrowed = super::external_adapters::package_adapter_export_bindings_for_kind(
        &enriched,
        ModuleId(2),
        bindings,
        super::external_adapters::ExternalPackageAdapterKind::CommonJsWrapper,
        None,
    );

    assert_eq!(narrowed, BTreeSet::from([BindingName::new("packageInit")]));
}

#[test]
fn external_adapter_preserves_unproven_commonjs_named_exports() {
    let planner = ImportExportPlanner;
    let app_source = "var value = packageInit().answer;\nexport { value };\n";
    let package_source = r#"
        var packageInit = (() => {
            let _$cached;
            return () => {
                if (_$cached) return _$cached.exports;
                var _$module = _$cached = { exports: {} };
                ((exports) => { exports.answer = 42; })(_$module.exports, _$module);
                return _$module.exports;
            };
        })();
        export { packageInit };
    "#;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        ));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let enriched = enriched_from_rows(rows);

    let analysis = super::PlannerAnalysis::from_program(&enriched);
    assert!(
        !analysis
            .external_package_adapters
            .contains_key(&ModuleId(2)),
        "named CommonJS object exports need package-member proof before source replacement"
    );
    assert!(!analysis.source_suppressed_packages.contains(&ModuleId(2)));

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_source = planned_source(&plan, "modules/package.ts");
    assert!(package_source.contains("_$cached"));
    assert!(!package_source.contains("external_fixture_package"));
}

#[test]
fn external_adapter_detects_commonjs_wrapper_without_synthetic_export() {
    let package_source = r#"
        var packageInit = (() => {
            let _$cached;
            return () => {
                if (_$cached) return _$cached.exports;
                var _$module = _$cached = { exports: {} };
                ((exports) => { exports.answer = 42; })(_$module.exports, _$module);
                return _$module.exports;
            };
        })();
    "#;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        ));
    let enriched = enriched_from_rows(rows);

    let kind = super::external_adapters::external_package_adapter_kind(
        &enriched,
        ModuleId(2),
        &BTreeSet::from([BindingName::new("packageInit")]),
    );

    assert_eq!(
        kind,
        super::external_adapters::ExternalPackageAdapterKind::CommonJsWrapper
    );
}

#[test]
fn external_adapter_detects_commonjs_helper_alias_wrapper() {
    let package_source =
        "var packageInit = U((exports, module) => { module.exports.answer = 42; });";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        ));
    let enriched = enriched_from_rows(rows);

    let kind = super::external_adapters::external_package_adapter_kind(
        &enriched,
        ModuleId(2),
        &BTreeSet::from([BindingName::new("packageInit")]),
    );

    assert_eq!(
        kind,
        super::external_adapters::ExternalPackageAdapterKind::CommonJsWrapper
    );
}

#[test]
fn external_adapter_preserves_worker_asset_source() {
    let planner = ImportExportPlanner;
    let app_source = "packageInit();\nexport const value = 1;\n";
    let package_source =
        "var packageInit = U((exports, module) => { module.exports.answer = 42; });";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package/dist/file.worker.js",
        )
        .with_resolved_file("fixture-package@1.0.0/dist/file.worker.js"),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let enriched = enriched_from_rows(rows);

    let analysis = super::PlannerAnalysis::from_program(&enriched);
    assert!(
        !analysis
            .external_package_adapters
            .contains_key(&ModuleId(2)),
        "worker package assets must not be converted into eager adapter imports"
    );
    assert!(!analysis.source_suppressed_packages.contains(&ModuleId(2)));

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_source = planned_source(&plan, "modules/package.ts");
    assert!(package_source.contains("var packageInit = U("));
    assert!(!package_source.contains("external_fixture_package"));
}

#[test]
fn external_adapter_preserves_weak_graph_source_proof() {
    let planner = ImportExportPlanner;
    let app_source = "var value = packageInit()();\nexport { value };\n";
    let package_source = r#"
        var packageInit = (() => {
            let _$cached;
            return () => {
                if (_$cached) return _$cached.exports;
                var _$module = _$cached = { exports: {} };
                ((exports, module) => { module.exports = function() { return 42; }; })(_$module.exports, _$module);
                return _$module.exports;
            };
        })();
        export { packageInit };
    "#;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package/lib/internal.js",
        )
        .with_resolved_file(
            "forced-external:dependency-graph-source:dependency-neighborhood:graph=2/2:functions=0:strings=0:fixture-package@1.0.0/lib/internal.js",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let enriched = enriched_from_rows(rows);

    let analysis = super::PlannerAnalysis::from_program(&enriched);
    assert!(
        !analysis
            .external_package_adapters
            .contains_key(&ModuleId(2)),
        "dependency graph source hints are ownership suggestions, not exact adapter replacement proof"
    );
    assert!(!analysis.source_suppressed_packages.contains(&ModuleId(2)));

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_source = planned_source(&plan, "modules/package.ts");
    assert!(package_source.contains("_$cached"));
    assert!(!package_source.contains("external_fixture_package"));
}

#[test]
fn external_adapter_preserves_plain_package_cache_source_hint() {
    let planner = ImportExportPlanner;
    let app_source = "var value = packageInit().PublicApi;\nexport { value };\n";
    let package_source = r#"
        var packageInit = (() => {
            let _$cached;
            return () => {
                if (_$cached) return _$cached.exports;
                var _$module = _$cached = { exports: {} };
                ((exports, module) => { module.exports = { PublicApi: class PublicApi {} }; })(_$module.exports, _$module);
                return _$module.exports;
            };
        })();
        export { packageInit };
    "#;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package/lib/maybe.js",
        )
        .with_resolved_file("fixture-package@1.0.0/lib/maybe.js"),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let enriched = enriched_from_rows(rows);

    let analysis = super::PlannerAnalysis::from_program(&enriched);
    assert!(
        !analysis
            .external_package_adapters
            .contains_key(&ModuleId(2)),
        "plain package cache paths are suggestions until promoted to an exact proof"
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_source = planned_source(&plan, "modules/package.ts");
    assert!(package_source.contains("PublicApi"));
    assert!(!package_source.contains("external_fixture_package"));
}

#[test]
fn external_adapter_preserves_canonical_subpath_hint_without_equivalence_proof() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some("packageInit();".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some("var packageInit = U((exports, module) => { module.exports = 1; });".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package/unsafe/internal.js",
        )
        .with_resolved_file("forced-external:canonical-subpath:fixture-package@1.0.0/internal.js"),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let enriched = enriched_from_rows(rows);

    assert!(
        !super::PlannerAnalysis::from_program(&enriched)
            .external_package_adapters
            .contains_key(&ModuleId(2)),
        "canonical subpath hints prove a possible import path, not adapter source equivalence"
    );
    let source = planned_source(
        &planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize"),
        "modules/package.ts",
    );
    assert!(!source.contains("external_fixture_package"));
}

#[test]
fn external_adapter_allows_self_contained_subpath_identity_hint() {
    let planner = ImportExportPlanner;
    let app_source = "var value = mathematica();\nexport { value };\n";
    let package_source = "var mathematica = U((exports, module) => { module.exports = function() { return {}; }; });";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "mathematica",
            "modules/fixture-package/mathematica.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package/lib/plugins/mathematica.js",
        )
        .with_resolved_file("fixture-package@1.0.0/lib/plugins/mathematica.js"),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let enriched = enriched_from_rows(rows);

    assert!(
        super::PlannerAnalysis::from_program(&enriched)
            .external_package_adapters
            .contains_key(&ModuleId(2)),
        "self-contained package modules can use their matching package subpath"
    );
    let source = planned_source(
        &planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize"),
        "modules/fixture-package/mathematica.ts",
    );
    assert!(source.contains("external_fixture_package"));
    assert!(!source.contains("module.exports"));
}

#[test]
fn external_adapter_rejects_ambiguous_index_subpath_identity() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some("var value = packageInit();\nexport { value };\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some("var packageInit = U((exports, module) => { module.exports = 1; });".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/fixture-package/index.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package/lib/index.js",
        )
        .with_resolved_file("fixture-package@1.0.0/lib/index.js"),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let enriched = enriched_from_rows(rows);

    assert!(
        !super::PlannerAnalysis::from_program(&enriched)
            .external_package_adapters
            .contains_key(&ModuleId(2)),
        "generic entrypoint names do not prove subpath identity for weak source hints"
    );
}

#[test]
fn adapter_required_package_original_binding_stays_callable() {
    let planner = ImportExportPlanner;
    let app_source = "packageInit();\nexport const value = 1;\n";
    let package_source = "var packageInit = E(() => {});\n";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        )
        .with_resolved_file(
            "exact-hint:fixture-package@1.0.0:quality=trusted:semantic_path=modules/package.ts",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/package.ts")
        .expect("adapter package file should be emitted");
    let source = package_file.body.join("\n");

    assert!(source.contains("function packageInit() { return external_fixture_package; }"));
    assert!(!source.contains("const packageInit = external_fixture_package;"));
}

#[test]
fn adapter_required_package_ignores_unused_exported_bindings() {
    let planner = ImportExportPlanner;
    let app_source =
        "function render(A) { let q = 1; return A + q; }\npackageInit();\nexport { render };\n";
    let package_source =
        "var A;\nvar q;\nvar packageInit = E(() => {});\nexport { A, q, packageInit };\n";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        )
        .with_resolved_file(
            "exact-hint:fixture-package@1.0.0:quality=trusted:semantic_path=modules/package.ts",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let analysis = super::PlannerAnalysis::from_program(&enriched);
    assert!(
        analysis
            .external_package_adapters
            .contains_key(&ModuleId(2)),
        "unused exported locals must not prevent adapter generation"
    );
    assert!(analysis.source_suppressed_packages.contains(&ModuleId(2)));

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_source = planned_source(&plan, "modules/package.ts");
    assert!(package_source.contains("function packageInit() { return external_fixture_package; }"));
    assert!(package_source.contains("export { packageInit };"));
    assert!(!package_source.contains("export { A"));
    assert!(!package_source.contains("var A"));
    assert!(!package_source.contains("var q"));
}

#[test]
fn commonjs_reexport_proof_allows_original_binding_adapter_despite_noisy_source() {
    let planner = ImportExportPlanner;
    let app_source = "packageInit();\nexport const value = 1;\n";
    let package_source = "function noisySideEffect() { return Date.now(); }\nnoisySideEffect();\nvar packageInit = E(() => {});\nexport { packageInit };\n";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        )
        .with_resolved_file(
            "forced-external:export-members:commonjs-reexport:PublicApi:fixture-package@1.0.0/index.js",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let analysis = super::PlannerAnalysis::from_program(&enriched);
    assert!(
        analysis
            .external_package_adapters
            .contains_key(&ModuleId(2)),
        "commonjs reexport proof should allow replacing noisy source when only original binding is requested"
    );
    assert!(analysis.source_suppressed_packages.contains(&ModuleId(2)));

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_source = planned_source(&plan, "modules/package.ts");
    assert!(package_source.contains("function packageInit() { return external_fixture_package; }"));
    assert!(package_source.contains("export { packageInit };"));
    assert!(!package_source.contains("noisySideEffect"));
}

#[test]
fn adapter_required_external_package_suppresses_original_runtime_source() {
    let planner = ImportExportPlanner;
    let app_source = "packageInit();\nexport const value = 1;\n";
    let package_source = "var packageInit = lazyValue(() => 1);\nexport { packageInit };\n";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "package",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        )
        .with_resolved_file(
            "exact-hint:fixture-package@1.0.0:quality=trusted:semantic_path=modules/package.ts",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let analysis = super::PlannerAnalysis::from_program(&enriched);
    assert!(
        analysis
            .external_package_adapters
            .contains_key(&ModuleId(2))
    );
    assert!(analysis.source_suppressed_packages.contains(&ModuleId(2)));
    assert!(
        !analysis.lowered_runtime_sources.contains_key(&ModuleId(2)),
        "adapter packages replace the original source and must not own migrated runtime helpers"
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/package.ts")
        .expect("adapter package file should be emitted");
    let source = package_file.body.join("\n");
    assert!(source.contains("function packageInit() { return external_fixture_package; }"));
    assert!(!source.contains("lazyValue"));
}

#[test]
fn adapter_required_package_with_argument_callable_binding_preserves_source() {
    let planner = ImportExportPlanner;
    let app_source = "packageInit();\nconst value = memoize(() => 1);\nexport { value };\n";
    let package_source = "var packageInit = E(() => {});\nfunction memoize(fn) { return fn; }\n";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        ));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let analysis = super::PlannerAnalysis::from_program(&enriched);
    assert!(
        !analysis
            .external_package_adapters
            .contains_key(&ModuleId(2))
    );
    assert!(!analysis.source_suppressed_packages.contains(&ModuleId(2)));

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/package.ts")
        .expect("source-preserved package file should be emitted");
    let source = package_file.body.join("\n");
    assert!(source.contains("function memoize(fn)"));
    assert!(source.contains("export { memoize, packageInit };"));
    assert!(!source.contains("external_fixture_package"));
}

#[test]
fn export_member_adapter_proof_allows_callable_binding_with_arguments() {
    let planner = ImportExportPlanner;
    let app_source = "packageInit();\nconst value = memoize(() => 1);\nexport { value };\n";
    let package_source = "var packageInit = E(() => {});\nfunction memoize(fn) { return fn; }\n";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        )
        .with_resolved_file(
            "forced-external:export-members:barrel-reference:memoize:fixture-package@1.0.0/dist/index.js",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let analysis = super::PlannerAnalysis::from_program(&enriched);
    assert!(
        analysis
            .external_package_adapters
            .contains_key(&ModuleId(2))
    );
    assert!(analysis.source_suppressed_packages.contains(&ModuleId(2)));

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/package.ts")
        .expect("adapter package file should be emitted");
    let source = package_file.body.join("\n");
    assert!(source.contains("function packageInit() { return external_fixture_package; }"));
    assert!(source.contains("const memoize = external_fixture_package.memoize;"));
    assert!(source.contains("export { memoize, packageInit };"));
    assert!(!source.contains("function memoize(fn)"));
}

#[test]
fn export_member_adapter_parses_export_all_reexport_proof() {
    let attribution = PackageAttributionInput::accepted_external(
        ModuleId(2),
        "fixture-package",
        "1.0.0",
        "fixture-package",
    )
    .with_resolved_file(
        "forced-external:export-members:export-all-reexport:PublicWidget:fixture-package@1.0.0/dist/index.js",
    );

    let proof =
        export_member_adapter_proof(&attribution).expect("export-all reexport proof should parse");

    assert_eq!(proof.kind, ExportMemberAdapterProofKind::ExportAllReexport);
    assert!(proof.exported_members.contains("PublicWidget"));
}

#[test]
fn export_member_adapter_maps_local_cjs_export_alias_to_external_member() {
    let planner = ImportExportPlanner;
    let app_source = "const value = new LocalClient();\nexport { value };\n";
    let package_source = "var packageInit = E(() => {});\nclass LocalClient {}\nexports.PublicClient = LocalClient;\n";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        )
        .with_resolved_file(
            "forced-external:export-members:barrel-reference:PublicClient:fixture-package@1.0.0/dist/index.js",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_source = planned_source(&plan, "modules/package.ts");
    assert!(
        package_source.contains("const LocalClient = external_fixture_package.PublicClient;"),
        "{package_source}"
    );
    assert!(!package_source.contains("class LocalClient"));
}

#[test]
fn export_member_adapter_alias_proof_elides_lazy_runtime_member_writes() {
    let planner = ImportExportPlanner;
    let app_source =
        "packageInit();\nconst value = new C();\nconst code = q.alpha;\nexport { value, code };\n";
    let package_source = concat!(
        "var q, C;\n",
        "var packageInit = E(() => {\n",
        "  depInit();\n",
        "  q = arrayToEnum([\"alpha\", \"beta\", \"gamma\"]);\n",
        "  C = class C extends Error { constructor() { super(); this.name = \"PublicError\"; } };\n",
        "});\n",
    );
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        )
        .with_resolved_file(
            "forced-external:export-members:source-equivalent:ErrorCode,PublicError:aliases=q=ErrorCode:fixture-package@1.0.0/dist/index.js",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let analysis = super::PlannerAnalysis::from_program(&enriched);
    assert!(
        analysis
            .external_package_adapters
            .contains_key(&ModuleId(2)),
        "source-equivalent alias proof should allow adapter despite lazy init calls/writes"
    );
    assert!(analysis.source_suppressed_packages.contains(&ModuleId(2)));

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_source = planned_source(&plan, "modules/package.ts");
    assert!(package_source.contains("function packageInit() { return external_fixture_package; }"));
    assert!(package_source.contains("const C = external_fixture_package.PublicError;"));
    assert!(package_source.contains("const q = external_fixture_package.ErrorCode;"));
    assert!(!package_source.contains("depInit"));
    assert!(!package_source.contains("arrayToEnum"));
}

#[test]
fn runtime_helper_imports_adapter_owned_member_aliases() {
    let planner = ImportExportPlanner;
    let prelude = "var q;\nvar C;\nfunction useRuntime() { packageInit(); return [C, q.alpha]; }\n";
    let body = "packageInit();\nconst value = useRuntime();\nexport { value };\n";
    let source = format!("{prelude}{body}");
    let package_source = concat!(
        "var q, C;\n",
        "var packageInit = E(() => {\n",
        "  q = arrayToEnum([\"alpha\", \"beta\", \"gamma\"]);\n",
        "  C = class C extends Error { constructor() { super(); this.name = \"PublicError\"; } };\n",
        "});\n",
    );
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        )
        .with_resolved_file(
            "forced-external:export-members:source-equivalent:ErrorCode,PublicError:aliases=q=ErrorCode:fixture-package@1.0.0/dist/index.js",
        ),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");
    assert!(
        helper_source.contains("import { C, packageInit, q } from '../package.js';"),
        "{helper_source}"
    );
    assert!(!helper_source.contains("var q;"));
    assert!(!helper_source.contains("var C;"));
    assert!(helper_source.contains("return [C, q.alpha];"));
}

#[test]
fn adapter_required_package_with_global_side_effect_preserves_source() {
    let planner = ImportExportPlanner;
    let app_source = "packageInit();\nexport const value = runtimeValue;\n";
    let package_source = "var packageInit = E(() => { runtimeValue = 1; });\n";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "app.js",
        Some(app_source.to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some(package_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "packageInit",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        ));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let analysis = super::PlannerAnalysis::from_program(&enriched);
    assert!(
        !analysis
            .external_package_adapters
            .contains_key(&ModuleId(2))
    );
    assert!(!analysis.source_suppressed_packages.contains(&ModuleId(2)));

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let package_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/package.ts")
        .expect("source-preserved package file should be emitted");
    let source = package_file.body.join("\n");
    assert!(source.contains("runtimeValue = 1"));
    assert!(!source.contains("external_fixture_package"));
}

#[test]
fn external_adapter_treats_minified_thunk_factory_initializer_as_callable() {
    assert!(compact_source_defines_callable_binding(
        "varrP7=E(()=>{v3();});",
        "rP7"
    ));
    assert!(compact_source_defines_callable_binding(
        "constinit=lazy(()=>{});",
        "init"
    ));
    assert!(!compact_source_defines_callable_binding(
        "varnotCallable=factory(value);",
        "notCallable"
    ));
}

#[test]
fn entrypoint_runtime_drops_noop_runtime_side_effects() {
    let planner = ImportExportPlanner;
    let prelude = "var noop = () => {};\nfunction main() { return 1; }\n";
    let body = "export const value = 1;\n";
    let tail = "noop();\nmain();\n";
    let source = format!("{prelude}{body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body.len()) as u32,
            )),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let cli_source = planned_source(&plan, "cli.ts");
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");

    assert!(!cli_source.contains("noop();"));
    assert!(!cli_source.contains("var noop"));
    assert!(!cli_source.contains("function main()"));
    assert!(!entrypoint_source.contains("noop();"));
    assert!(!entrypoint_source.contains("var noop"));
    assert!(entrypoint_source.contains("function main()"));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "noop-only entrypoint island should not keep runtime helper code"
    );
}

#[test]
fn entrypoint_runtime_keeps_non_noop_runtime_side_effect_dependencies() {
    let planner = ImportExportPlanner;
    let prelude =
        "var setup = () => { globalThis.ready = true; };\nfunction main() { return 1; }\n";
    let body = "export const value = 1;\n";
    let tail = "setup();\nmain();\n";
    let source = format!("{prelude}{body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body.len()) as u32,
            )),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let cli_source = planned_source(&plan, "cli.ts");
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");

    assert!(!cli_source.contains("var setup"));
    assert!(!cli_source.contains("setup();"));
    assert!(entrypoint_source.contains("var setup"));
    assert!(entrypoint_source.contains("setup();"));
    assert!(entrypoint_source.contains("globalThis.ready = true"));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "entrypoint-only side-effect dependencies should not keep runtime helper code"
    );
}

#[test]
fn runtime_import_identifier_scan_ignores_globals_and_property_names() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "function local() { function nested() {} }\n\
         let { isReady: localAlias } = source;\n\
         class Transport { buffer = Buffer.alloc(0); isBridge; async start() {} stop() {} }\n\
         const reader = (event) => event.ready;\n\
         const copy = [...shared];\n\
         as.command('servers');\n\
         console.log(request.method); Promise.resolve().then(() => (packageInit(), ns));",
    );

    assert!(identifiers.contains("as"));
    assert!(identifiers.contains("packageInit"));
    assert!(identifiers.contains("request"));
    assert!(identifiers.contains("ns"));
    assert!(identifiers.contains("source"));
    assert!(identifiers.contains("shared"));
    assert!(!identifiers.contains("console"));
    assert!(!identifiers.contains("log"));
    assert!(!identifiers.contains("method"));
    assert!(!identifiers.contains("Promise"));
    assert!(!identifiers.contains("resolve"));
    assert!(!identifiers.contains("then"));
    assert!(!identifiers.contains("local"));
    assert!(!identifiers.contains("nested"));
    assert!(!identifiers.contains("localAlias"));
    assert!(!identifiers.contains("buffer"));
    assert!(!identifiers.contains("isBridge"));
    assert!(!identifiers.contains("event"));
    assert!(!identifiers.contains("start"));
    assert!(!identifiers.contains("stop"));
}

#[test]
fn runtime_import_identifier_scan_ignores_catch_bindings() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "function reader() { try { return shared; } catch (_) { return _.code || fallback; } }",
    );

    assert!(identifiers.contains("shared"));
    assert!(identifiers.contains("fallback"));
    assert!(!identifiers.contains("_"));
    assert!(!identifiers.contains("code"));
}

#[test]
fn runtime_import_identifier_scan_ignores_function_arguments_object() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "function reader() { return arguments.length ? shared : fallback; }",
    );

    assert!(identifiers.contains("shared"));
    assert!(identifiers.contains("fallback"));
    assert!(!identifiers.contains("arguments"));
    assert!(!identifiers.contains("length"));
}

#[test]
fn runtime_import_identifier_scan_ignores_comma_sequence_locals() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "async function reader() { let y = await new Promise((R, I) => { let x = !1, B = (C) => { R(C); }; B(shared); }); return y; }",
    );

    assert!(identifiers.contains("shared"));
    assert!(!identifiers.contains("B"));
    assert!(!identifiers.contains("C"));
    assert!(!identifiers.contains("I"));
    assert!(!identifiers.contains("R"));
    assert!(!identifiers.contains("x"));
    assert!(!identifiers.contains("y"));
}

#[test]
fn runtime_import_identifier_scan_keeps_default_parameter_initializer_reads() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "class Z96 extends Promise {\n\
             constructor(q, K, _ = e38) { this.parseResponse = _; }\n\
             method({ value = fallback }, [item = other] = input) { return value + item; }\n\
         }",
    );

    assert!(identifiers.contains("e38"));
    assert!(identifiers.contains("fallback"));
    assert!(identifiers.contains("other"));
    assert!(identifiers.contains("input"));
    assert!(!identifiers.contains("q"));
    assert!(!identifiers.contains("K"));
    assert!(!identifiers.contains("_"));
    assert!(!identifiers.contains("value"));
    assert!(!identifiers.contains("item"));
    assert!(!identifiers.contains("Z96"));
    assert!(!identifiers.contains("constructor"));
    assert!(!identifiers.contains("method"));
    assert!(!identifiers.contains("Promise"));
}

#[test]
fn runtime_import_identifier_scan_keeps_member_call_callback_reads() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "function reader() { return Promise.resolve().catch(noop); }",
    );

    assert!(identifiers.contains("noop"));
    assert!(!identifiers.contains("Promise"));
    assert!(!identifiers.contains("resolve"));
    assert!(!identifiers.contains("catch"));
}

#[test]
fn runtime_import_identifier_scan_keeps_for_await_source_reads() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "async function* stream(q, K) {\n\
             for await (let A of HY5(q, K)) {\n\
                 yield A;\n\
             }\n\
         }",
    );

    assert!(identifiers.contains("HY5"));
    assert!(!identifiers.contains("stream"));
    assert!(!identifiers.contains("q"));
    assert!(!identifiers.contains("K"));
    assert!(!identifiers.contains("A"));
}

#[test]
fn local_binding_scan_keeps_default_initializer_reads_out_of_locals() {
    let locals = super::local_bindings_in_source(
        "function reader(a = dep, { x: alias = fallback }, [item = other] = input) {\n\
             const local = alias + item;\n\
             return local;\n\
         }",
    );

    assert!(locals.contains("reader"));
    assert!(locals.contains("a"));
    assert!(locals.contains("alias"));
    assert!(locals.contains("item"));
    assert!(locals.contains("local"));
    assert!(!locals.contains("dep"));
    assert!(!locals.contains("fallback"));
    assert!(!locals.contains("other"));
    assert!(!locals.contains("input"));
}

#[test]
fn runtime_import_identifier_scan_ignores_template_expression_arrow_params() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "function reader(K) { return Object.entries(K).map(([_, z]) => `# ${_}\n${z}`).join('\\n') + shared; }",
    );

    assert!(identifiers.contains("shared"));
    assert!(!identifiers.contains("_"));
    assert!(!identifiers.contains("z"));
    assert!(!identifiers.contains("K"));
}

#[test]
fn runtime_import_identifier_scan_ignores_web_platform_globals() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "const response = Response.json({ ok: true });\n\
         const encoded = btoa('x');\n\
         fetch('/ready');\n\
         new Event('ready');\n\
         document.dispatchEvent(new CustomEvent('ready'));\n\
         window.localStorage.getItem('ready');\n\
         new XMLHttpRequest();\n\
         Bun.file('ready');\n\
         unescape('%20');\n\
         new AggregateError([]);\n\
         new WeakRef({});\n\
         new FinalizationRegistry(() => {});\n\
         BigInt64Array.from([]);\n\
         BigUint64Array.from([]);\n\
         if (socket.readyState === WebSocket.OPEN) socket.send('x');\n\
         shared;",
    );

    assert!(identifiers.contains("shared"));
    assert!(!identifiers.contains("Response"));
    assert!(!identifiers.contains("btoa"));
    assert!(!identifiers.contains("fetch"));
    assert!(!identifiers.contains("Event"));
    assert!(!identifiers.contains("document"));
    assert!(!identifiers.contains("CustomEvent"));
    assert!(!identifiers.contains("window"));
    assert!(!identifiers.contains("localStorage"));
    assert!(!identifiers.contains("XMLHttpRequest"));
    assert!(!identifiers.contains("Bun"));
    assert!(!identifiers.contains("unescape"));
    assert!(!identifiers.contains("AggregateError"));
    assert!(!identifiers.contains("WeakRef"));
    assert!(!identifiers.contains("FinalizationRegistry"));
    assert!(!identifiers.contains("BigInt64Array"));
    assert!(!identifiers.contains("BigUint64Array"));
    assert!(!identifiers.contains("WebSocket"));
}

#[test]
fn runtime_import_identifier_scan_ignores_unreachable_post_return_function_tail() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "function reader(q) {\n\
            return runtimeDep(q);\n\
            function dead(value) { return missing(value) + other; }\n\
        }",
    );

    assert!(identifiers.contains("runtimeDep"));
    assert!(!identifiers.contains("q"));
    assert!(!identifiers.contains("dead"));
    assert!(!identifiers.contains("value"));
    assert!(!identifiers.contains("missing"));
    assert!(!identifiers.contains("other"));
}

#[test]
fn runtime_import_identifier_scan_keeps_post_return_function_tail_when_referenced() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "function reader(q) {\n\
            return dead(q);\n\
            function dead(value) { return missing(value); }\n\
        }",
    );

    assert!(identifiers.contains("missing"));
    assert!(!identifiers.contains("q"));
    assert!(!identifiers.contains("dead"));
    assert!(!identifiers.contains("value"));
}

#[test]
fn runtime_import_identifier_scan_keeps_deps_after_conditional_return() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "function reader(q) {\n\
            if (!guard(q) || ready(q)) return false;\n\
            var value = choose(q) ? left : right;\n\
            return done(normalize(value));\n\
        }",
    );

    assert!(identifiers.contains("guard"));
    assert!(identifiers.contains("ready"));
    assert!(identifiers.contains("choose"));
    assert!(identifiers.contains("left"));
    assert!(identifiers.contains("right"));
    assert!(identifiers.contains("done"));
    assert!(identifiers.contains("normalize"));
    assert!(!identifiers.contains("q"));
    assert!(!identifiers.contains("value"));
}

#[test]
fn runtime_import_identifier_scan_ignores_setter_class_expression_name() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "var init = lazyValue(() => { __reverts_set_$F(class $F extends qP8 { method() { return qP8; } }); });",
    );

    assert!(identifiers.contains("__reverts_set_$F"));
    assert!(identifiers.contains("qP8"));
    assert!(!identifiers.contains("$F"));
}

#[test]
fn runtime_import_identifier_scan_keeps_later_read_after_setter_class_expression() {
    let identifiers = super::runtime_import_identifiers_in_source(
        "var init = lazyValue(() => { __reverts_set_kr6(class kr6 {}); az.Hooks = kr6; });",
    );

    assert!(identifiers.contains("__reverts_set_kr6"));
    assert!(identifiers.contains("az"));
    assert!(identifiers.contains("kr6"));
}

#[test]
fn runtime_proxy_function_single_use_inlines_when_private() {
    let source = concat!(
        "function proxy(q, K) {\n",
        "\treturn target(q, K);\n",
        "}\n",
        "function use() { return proxy(1, 2); }\n",
    );
    let rewritten = super::inline_single_use_runtime_proxy_functions(source, &BTreeSet::new());

    assert!(!rewritten.contains("function proxy"));
    assert!(rewritten.contains("function use() { return target(1, 2); }"));
}

#[test]
fn runtime_proxy_arrow_single_use_inlines_when_private() {
    let source = concat!(
        "var proxy = (q, K) => target(q, K);\n",
        "var unary = q => targetOne(q);\n",
        "function use() { return proxy(1, 2) + unary(3); }\n",
    );
    let rewritten = super::inline_single_use_runtime_proxy_functions(source, &BTreeSet::new());

    assert!(!rewritten.contains("var proxy"));
    assert!(!rewritten.contains("var unary"));
    assert!(rewritten.contains("function use() { return target(1, 2) + targetOne(3); }"));
}

#[test]
fn runtime_proxy_function_keeps_exported_or_value_used_proxy() {
    let exported = concat!(
        "function proxy(q) {\n",
        "\treturn target(q);\n",
        "}\n",
        "function use() { return proxy(1); }\n",
    );
    let kept_exported = super::inline_single_use_runtime_proxy_functions(
        exported,
        &BTreeSet::from([BindingName::new("proxy")]),
    );
    assert!(kept_exported.contains("function proxy"));

    let value_used = concat!(
        "function proxy(q) {\n",
        "\treturn target(q);\n",
        "}\n",
        "var saved = proxy;\n",
    );
    let kept_value_used =
        super::inline_single_use_runtime_proxy_functions(value_used, &BTreeSet::new());
    assert!(kept_value_used.contains("function proxy"));
}

#[test]
fn runtime_proxy_function_keeps_async_proxy() {
    let source = concat!(
        "async /* generated */ function proxy(q) {\n",
        "\treturn target(q);\n",
        "}\n",
        "function use() { return proxy(1); }\n",
    );
    let rewritten = super::inline_single_use_runtime_proxy_functions(source, &BTreeSet::new());

    assert_eq!(rewritten, source);
}

#[test]
fn call_identifier_scan_keeps_direct_initializer_calls_with_local_name_collision() {
    let identifiers = super::call_identifiers_in_source(
        "const local = ({ e3 }) => e3;\n\
         e3();\n\
         object.e3();\n\
         function e3Local() {}\n",
    );

    assert!(identifiers.contains("e3"));
    assert!(!identifiers.contains("e3Local"));
}

#[test]
fn class_fields_are_not_implicit_global_writes() {
    let writes = super::implicit_global_writes_in_source(
        "class Transport { isBridge = false; ready; method() { this.ready = true; } }\n\
         shared = 1;",
    );

    assert!(writes.contains(&BindingName::new("shared")));
    assert!(!writes.contains(&BindingName::new("isBridge")));
    assert!(!writes.contains(&BindingName::new("ready")));
}

#[test]
fn singleton_inline_closure_keeps_recursive_prelude_dependencies() {
    let planner = ImportExportPlanner;
    let prelude = concat!(
        "function first() { return second(); }\n",
        "function second() { return third(); }\n",
        "function third() { return 3; }\n",
    );
    let body = "var value = first();\nexport { value };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let entry_source = entry_file.body.join("\n");

    assert!(entry_source.contains("function first()"));
    assert!(entry_source.contains("function second()"));
    assert!(entry_source.contains("function third()"));
    assert!(!entry_source.contains("source-1-helpers"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn singleton_inline_rejects_dependency_already_owned_by_migration() {
    let source = "\
function root() { return migratedDep(); }\n\
function migratedDep() { return 1; }\n";
    let mut offset = 0u32;
    let mut snippet = |text: &str| {
        let byte_start = offset;
        offset += text.len() as u32 + 1;
        RuntimePreludeSnippet {
            source: text.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
        }
    };
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: source.to_string(),
        bindings: BTreeMap::from([
            (
                BindingName::new("root"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
            (
                BindingName::new("migratedDep"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]),
        snippets: BTreeMap::from([
            (
                BindingName::new("root"),
                snippet("function root() { return migratedDep(); }"),
            ),
            (
                BindingName::new("migratedDep"),
                snippet("function migratedDep() { return 1; }"),
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let read_index = super::runtime_source_read_index(&prelude, &[]);
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some(source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(7), "consumer", "modules/consumer.ts")
            .with_source_file(1),
    );
    let program = enriched_from_rows(rows);
    let mut runtime_var_migrations = super::RuntimeVarMigrationPlan::default();
    runtime_var_migrations.insert_owned_snippet(
        BindingName::new("migratedDep"),
        super::RuntimeOwnedSnippetMigration {
            owner_module: ModuleId(7),
            source_file_id: 1,
            extra_runtime_deps: BTreeSet::new(),
            extra_runtime_dep_aliases: BTreeMap::new(),
            extra_noop_deps: BTreeSet::new(),
            moves_namespace_export: false,
        },
    );
    let consumers_by_binding =
        BTreeMap::from([((1, BindingName::new("root")), BTreeSet::from([ModuleId(7)]))]);

    let ctx = super::runtime_singleton_inline::RuntimeSingletonInlineContext {
        program: &program,
        lowered_runtime_sources: &BTreeMap::new(),
        runtime_var_migrations: &runtime_var_migrations,
        prelude: &prelude,
        read_index: &read_index,
        source_file_id: 1,
        consumers_by_binding: &consumers_by_binding,
        blocked_bindings: &BTreeSet::new(),
        direct_prelude_imports: None,
        source_definition_modules: &BTreeMap::new(),
        source_exported_bindings_by_module: &BTreeMap::new(),
        module_dependencies_by_owner: &BTreeMap::new(),
    };

    assert!(
        super::runtime_singleton_inline::resolve_runtime_singleton_inline_snippet(
            &ctx,
            &BindingName::new("root"),
            ModuleId(7)
        )
        .is_none(),
        "singleton inlining must not duplicate a helper already owned by a runtime migration"
    );
}

#[test]
fn lazy_folded_source_keeps_prelude_dependencies_used_by_folded_chunk() {
    let planner = ImportExportPlanner;
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared = Date.now();\n",
        "function buildShared() { return 42; }\n",
    );
    let body = concat!(
        "var initShared = lazy(() => { shared = buildShared(); });\n",
        "export { initShared, shared };\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let helper_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
        .expect("runtime helper file should be planned");
    let helper_source = helper_file.body.join("\n");

    assert!(helper_source.contains("function buildShared()"));
    assert!(helper_source.contains("shared = buildShared();"));
    assert!(helper_source.contains("var initShared = lazyValue(() => {"));
    assert!(helper_source.contains("import { lazyValue } from './lazy.js';"));
    assert!(helper_source.contains("export { initShared, shared };"));
    let lazy_source = planned_source(&plan, "modules/runtime/lazy.ts");
    assert!(lazy_source.contains("function lazyValue(factory) {"));
    assert!(lazy_source.contains("export { lazyValue };"));
    assert!(
        !helper_source.contains("import { initShared }"),
        "folded chunk definitions are local to the runtime helper and must not be re-imported"
    );
}

#[test]
fn self_contained_lazy_writer_stays_local_and_eliminates_runtime_setter() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
    );
    let writer_body = concat!(
        "var initShared = lazy(() => { shared = 'ok'; });\n",
        "export { initShared, shared };\n",
    );
    let consumer_body = "var value = initShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    // The writer stays LOCAL (no shared runtime helper) and the cross-module
    // setter is eliminated — its primary guarantees. Additionally, the global
    // de-lazify post-pass hoists `initShared` (body `shared = 'ok';`, no
    // top-level return, and invoked by the consumer) to eager module-eval and
    // stubs it; `var value = initShared()` was already `undefined` and stays so.
    // The dead local `_$l` memoizer is dropped.
    assert!(!writer_source.contains("_$l"), "{writer_source}");
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        writer_source.contains("function initShared() {}"),
        "{writer_source}"
    );
    assert!(!writer_source.contains("lazyValue("), "{writer_source}");
    assert!(!writer_source.contains("__reverts_set_shared"));
    assert!(consumer_source.contains("import { initShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn lazy_writer_with_movable_reader_stays_local_and_eliminates_runtime_setter() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "function readShared() { return shared; }\n",
    );
    let writer_body = concat!(
        "var initShared = lazy(() => { shared = 'ok'; });\n",
        "export { initShared, shared };\n",
    );
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(writer_source.contains("var _$l"), "{writer_source}");
    assert!(
        writer_source.contains("function readShared() { return shared; }"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("var initShared = _$l(() => {\n\tshared = 'ok';\n});"),
        "{writer_source}"
    );
    assert!(!writer_source.contains("__reverts_set_shared"));
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn lazy_writer_with_reader_runtime_dep_stays_local_and_eliminates_runtime_setter() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var suffix = '!';\n",
        "function readShared() { return shared + suffix; }\n",
    );
    let writer_body = concat!(
        "var initShared = lazy(() => { shared = 'ok'; });\n",
        "export { initShared, shared };\n",
    );
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        writer_source.contains("import { suffix } from './runtime/source-1-helpers.js';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source.contains("function readShared() { return shared + suffix; }"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("var initShared = _$l(() => {\n\tshared = 'ok';\n});"),
        "{writer_source}"
    );
    assert!(!writer_source.contains("lazyValue"), "{writer_source}");
    assert!(
        !writer_source.contains("__reverts_set_shared"),
        "{writer_source}"
    );
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(!helper_source.contains("var shared;"), "{helper_source}");
    assert!(
        !helper_source.contains("function readShared()"),
        "{helper_source}"
    );
    assert!(
        !helper_source.contains("__reverts_set_shared"),
        "{helper_source}"
    );
    assert!(
        !helper_source.contains("initShared = lazyValue"),
        "{helper_source}"
    );
}

#[test]
fn lazy_writer_localizes_lazy_value_with_safe_runtime_dep() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var suffix = Date.now();\n",
    );
    let body = concat!(
        "function readSuffix() { return suffix; }\n",
        "var initShared = lazy(() => { shared = readSuffix(); });\n",
        "export { initShared, shared };\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");

    assert!(
        writer_source.contains("import { suffix } from './runtime/source-1-helpers.js';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(writer_source.contains("var _$l"), "{writer_source}");
    assert!(
        writer_source.contains("var initShared = _$l(() => {"),
        "{writer_source}"
    );
    assert!(!writer_source.contains("lazyValue"), "{writer_source}");
    assert!(
        !writer_source.contains("__reverts_set_shared"),
        "{writer_source}"
    );
}

#[test]
fn self_contained_lazy_writer_localizes_lazy_value_after_inlining_private_runtime_dep() {
    let planner = ImportExportPlanner;
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "function buildShared() { return 42; }\n",
    );
    let body = concat!(
        "var initShared = lazy(() => { shared = buildShared(); });\n",
        "export { initShared, shared };\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(!entry_source.contains("source-1-helpers"), "{entry_source}");
    assert!(entry_source.contains("var shared;"), "{entry_source}");
    assert!(
        entry_source.contains("function buildShared() { return 42; }"),
        "{entry_source}"
    );
    assert!(entry_source.contains("var _$l"), "{entry_source}");
    assert!(
        entry_source.contains("var initShared = _$l(() => {\n\tshared = buildShared();\n});"),
        "{entry_source}"
    );
    assert!(!entry_source.contains("lazyValue("), "{entry_source}");
    assert!(!entry_source.contains("__reverts_set_shared"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn folded_non_snippet_runtime_read_can_import_migrated_writer_binding() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var used = Date.now();\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let folded_body = concat!(
        "var initUse = lazy(() => { used = shared; });\n",
        "export { initUse, used };\n",
    );
    let consumer_body = "var value = initUse();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        helper_source.contains("import { shared } from '../writer.js';"),
        "{helper_source}"
    );
    assert!(!helper_source.contains("var shared;"), "{helper_source}");
    assert!(
        !helper_source.contains("__reverts_set_shared"),
        "{helper_source}"
    );
    assert!(helper_source.contains("used = shared;"), "{helper_source}");
}

#[test]
fn runtime_helper_self_audit_rejects_unresolved_references() {
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: String::new(),
        bindings: BTreeMap::from([(
            BindingName::new("missingHelper"),
            RuntimePreludeBindingKind::SourceBacked,
        )]),
        snippets: BTreeMap::new(),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let unresolved = super::unresolved_runtime_helper_references(
        &prelude,
        "function main() { return missingHelper(); }\n",
        &BTreeSet::new(),
        &BTreeMap::new(),
    );

    assert_eq!(
        unresolved,
        BTreeSet::from([BindingName::new("missingHelper")])
    );
}

#[test]
fn module_dependencies_import_unresolved_source_bindings() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "entry.js",
        Some("var value = (() => helper())();\nexport { value };".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "helper.js",
        Some("function helper() { return 1; }".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "helper",
            "modules/helper.ts",
            "fixture-helper",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions
        .push(PackageAttributionInput::rejected_source(
            ModuleId(2),
            "fixture-helper",
            "fixture helper stays source-backed in planner fixture",
        ));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let helper_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/helper.ts")
        .expect("helper file should be planned");

    assert!(
        entry_file
            .body
            .join("\n")
            .contains("import { helper } from './helper.js';")
    );
    assert!(helper_file.body.join("\n").contains("export { helper };"));
}

#[test]
fn accepted_external_package_with_source_read_is_emitted_locally() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "entry.js",
        Some("packageInit();\nexport const value = 1;".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "package.js",
        Some("function packageInit() { return 1; }".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "package",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(2),
            "fixture-package",
            "1.0.0",
            "fixture-package",
        ));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let package_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/package.ts")
        .expect("source-read package file should be emitted");

    assert!(
        entry_file
            .body
            .join("\n")
            .contains("import { packageInit } from './package.js';")
    );
    assert!(
        package_file
            .body
            .join("\n")
            .contains("export { packageInit };")
    );
}

#[test]
fn source_suppressed_package_closure_requires_private_ownership_proof() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "root.js",
        Some("export const root = 1;".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "private.js",
        Some("export const privateValue = 1;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(1),
            "root",
            "modules/pkg-root.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "private",
            "modules/pkg-private.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(1),
            "pkg",
            "1.0.0",
            "pkg",
        ));
    rows.package_attributions
        .push(PackageAttributionInput::rejected_source(
            ModuleId(2),
            "pkg",
            "package matcher did not produce an accepted attribution for this package",
        ));
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let analysis = PlannerAnalysis::from_program(&enriched);

    assert!(analysis.source_suppressed_packages.contains(&ModuleId(1)));
    assert!(
        !analysis.source_suppressed_packages.contains(&ModuleId(2)),
        "private package source must not be suppressed without an ownership proof row"
    );
}

#[test]
fn accepted_external_package_suppresses_private_dependency_closure() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "root.js",
        Some("export const root = 1;".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "private.js",
        Some("export const privateValue = 1;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(1),
            "root",
            "modules/pkg-root.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "private",
            "modules/pkg-private.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(1),
            "pkg",
            "1.0.0",
            "pkg",
        ));
    let mut private_ownership = PackageAttributionInput::rejected_source(
        ModuleId(2),
        "pkg",
        "private dependency is covered by externalized package closure",
    );
    private_ownership.package_version = Some("1.0.0".to_string());
    rows.package_attributions.push(private_ownership);
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let analysis = PlannerAnalysis::from_program(&enriched);
    assert!(analysis.source_suppressed_packages.contains(&ModuleId(1)));
    assert!(
        analysis.source_suppressed_packages.contains(&ModuleId(2)),
        "private package dependencies reachable only from an externalized package root should be suppressed as the same closure"
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    assert!(
        plan.files
            .iter()
            .all(|file| file.path != "modules/pkg-private.ts"),
        "the private dependency closure must not be emitted as preserved source"
    );
}

#[test]
fn source_suppressed_package_closure_keeps_private_internal_for_cross_package_source_consumer() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "root.js",
        Some("export const root = 1;".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "private.js",
        Some("export const privateValue = 1;".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "consumer.js",
        Some("export const consumer = privateValue;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(1),
            "root",
            "modules/pkg-a-root.ts",
            "pkg-a",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "privateValue",
            "modules/pkg-a-private.ts",
            "pkg-a",
            Some("1.0.0".to_string()),
        )
        .with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(3),
            "consumer",
            "modules/pkg-b-consumer.ts",
            "pkg-b",
            Some("1.0.0".to_string()),
        )
        .with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(1),
            "pkg-a",
            "1.0.0",
            "pkg-a",
        ));
    let mut private_ownership = PackageAttributionInput::rejected_source(
        ModuleId(2),
        "pkg-a",
        "private dependency is covered by externalized package closure",
    );
    private_ownership.package_version = Some("1.0.0".to_string());
    rows.package_attributions.push(private_ownership);
    let mut boundary_consumer = PackageAttributionInput::rejected_source(
        ModuleId(3),
        "pkg-b",
        "different package consumer is preserved source",
    );
    boundary_consumer.package_version = Some("1.0.0".to_string());
    rows.package_attributions.push(boundary_consumer);
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let analysis = PlannerAnalysis::from_program(&enriched);

    // `privateValue` (ModuleId 2) is a pkg-a *internal*: rejected, with no
    // external import specifier. A cross-package source-preserved consumer
    // (pkg-b, ModuleId 3) directly references its binding. The boundary
    // exemption — "a different-package consumer re-imports it from its package,
    // so the internal can be suppressed" — is INVALID here: a non-public
    // internal can never be re-imported, so the kept pkg-b source would dangle
    // (this is the real-world ws `buffer-util`/`permessage-deflate` ->
    // `EI6 is not defined` failure, where a ws internal mis-attributed into
    // another package's closure was wrongly suppressed). The internal must stay
    // vendored.
    assert!(
        !analysis.source_suppressed_packages.contains(&ModuleId(2)),
        "a non-externally-providable internal directly read by a kept consumer must not be suppressed"
    );
}

#[test]
fn accepted_external_package_read_from_runtime_helper_uses_external_adapter() {
    let planner = ImportExportPlanner;
    let package_source = "var dNq = { default: () => 1 };\nvar cNq = lazyValue(() => dNq);\n";
    let app_source = "var app = lazyValue(() => (cNq(), dNq).default());\n";
    let entrypoint = "app();\n";
    let source = format!("{package_source}{app_source}{entrypoint}");
    let package_start = 0;
    let package_end = package_source.len() as u32;
    let app_start = package_end;
    let app_end = package_end + app_source.len() as u32;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source)));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(1),
            "open",
            "modules/open.ts",
            "open",
            Some("10.2.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(package_start, package_end)),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(app_start, app_end)),
    );
    rows.package_attributions.push(
        PackageAttributionInput::accepted_external(ModuleId(1), "open", "10.2.0", "open/index.js")
            .with_resolved_file(
                "exact-hint:open@10.2.0:quality=trusted:semantic_path=modules/open.ts",
            ),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let accepted_externalized_packages =
        accepted_external_module_ids(&enriched.model().input().package_attributions);
    let source_facts = super::SourceModuleFacts::from_program(&enriched);
    let adapter_analysis = super::external_adapters::external_package_adapter_analysis(
        &enriched,
        &accepted_externalized_packages,
        &source_facts,
    );
    assert!(
        adapter_analysis.adapters.contains_key(&ModuleId(1)),
        "package bindings read by source without an import edge must get an adapter"
    );
    let scan = super::scan_runtime_externalized_bindings(
        &enriched,
        "await (cNq(), dNq).default();",
        &BTreeSet::new(),
        &accepted_externalized_packages,
    );
    let init_shims = scan.package_init_shims;
    assert!(init_shims.contains(&BindingName::new("cNq")));
    assert!(!init_shims.contains(&BindingName::new("dNq")));

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("runtime helper package reads should be wired");

    let package_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/open.ts")
        .expect("runtime-read package adapter file should be emitted");
    let package_source = package_file.body.join("\n");
    assert_eq!(package_file.imports.len(), 1);
    assert_eq!(
        package_file.imports[0].resolution.specifier(),
        Some("open/index.js")
    );
    assert!(package_source.contains("function cNq() { return external_open; }"));
    assert!(package_source.contains("const dNq = external_open;"));
    assert!(package_source.contains("export { cNq, dNq };"));
    assert!(!package_source.contains("lazyValue"));
}

#[test]
fn cache_surface_public_entry_frees_internal_dependency_from_adapter_required() {
    // A package's public entry (specifier accepted by a cache-anchored public
    // surface) is replaced by a bare `import from "pkg"`, so it must NOT pin its
    // internal dependency as adapter-required: the import already provides it,
    // making the internal eliminable. Without the surface the internal stays
    // required (the pre-fix fault mode). The behaviour is driven solely by the
    // surface — no package is hard-coded.
    fn internal_is_adapter_required(with_surface: bool) -> (bool, bool) {
        let internal_src = "var intHelper = () => 1;\n";
        let entry_src = "var pubFn = () => intHelper();\n";
        let app_src = "pubFn();\n";
        let source = format!("{internal_src}{entry_src}{app_src}");
        let internal_end = internal_src.len() as u32;
        let entry_end = internal_end + entry_src.len() as u32;
        let app_end = entry_end + app_src.len() as u32;

        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source)));
        rows.modules.push(
            ModuleInput::package(
                ModuleId(1),
                "pubFn",
                "modules/entry.ts",
                "pkg",
                Some("1.0.0".to_string()),
            )
            .with_source_file(1)
            .with_source_span(SourceSpan::new(internal_end, entry_end)),
        );
        rows.modules.push(
            ModuleInput::package(
                ModuleId(2),
                "intHelper",
                "modules/internal.ts",
                "pkg",
                Some("1.0.0".to_string()),
            )
            .with_source_file(1)
            .with_source_span(SourceSpan::new(0, internal_end)),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "app", "modules/app.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(entry_end, app_end)),
        );
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(1),
                "pkg",
                "1.0.0",
                "pkg",
            ));
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(2),
                "pkg",
                "1.0.0",
                "pkg/internal/helper.js",
            ));
        if with_surface {
            rows.package_surfaces.push(PackageSurfaceInput {
                package_name: "pkg".to_string(),
                package_version: Some("1.0.0".to_string()),
                export_specifier: "pkg".to_string(),
                status: PackageAttributionStatus::Accepted,
                evidence: None,
            });
        }

        let enriched = enriched_from_rows(rows);
        let accepted = accepted_external_module_ids(&enriched.model().input().package_attributions);
        let source_facts = super::SourceModuleFacts::from_program(&enriched);
        let required = super::external_adapters::adapter_required_package_modules(
            &enriched,
            &accepted,
            &source_facts,
            &BTreeSet::new(),
        );
        (
            required.contains(&ModuleId(1)),
            required.contains(&ModuleId(2)),
        )
    }

    // Fault mode: with no public surface, the internal dependency is pinned.
    let (entry_no_surface, internal_no_surface) = internal_is_adapter_required(false);
    assert!(
        entry_no_surface,
        "public entry is pinned by the application read in both cases",
    );
    assert!(
        internal_no_surface,
        "without a public surface the internal stays adapter-required (pre-fix fault mode)",
    );

    // Fix: a cache-anchored public surface frees the internal for elimination.
    let (entry_with_surface, internal_with_surface) = internal_is_adapter_required(true);
    assert!(
        entry_with_surface,
        "public entry must still be adapter-required (app imports it)",
    );
    assert!(
        !internal_with_surface,
        "with a public surface, the internal dependency is no longer adapter-required",
    );
}

#[test]
fn source_preserved_public_entry_repins_its_externalized_internal() {
    // A publicly-importable entry is freed of its internal ONLY because it is
    // replaced by a bare `import from "pkg"` (source dropped). When no adapter
    // can be built for it, the entry is *source-preserved* — its real source is
    // kept and still references the internal — so the internal must be re-pinned
    // as adapter-required. This is the lodash `memoize`/`MapCache` (`vt`) crash:
    // memoize kept `new (qF1.Cache || vt)()`, but its bare-root-externalized
    // internal `MapCache` was suppressed, leaving `vt` an unassigned phantom.
    let internal_src = "var intHelper = () => 1;\n";
    let entry_src = "var pubFn = () => intHelper();\n";
    let app_src = "pubFn();\n";
    let source = format!("{internal_src}{entry_src}{app_src}");
    let internal_end = internal_src.len() as u32;
    let entry_end = internal_end + entry_src.len() as u32;
    let app_end = entry_end + app_src.len() as u32;

    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source)));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(1),
            "pubFn",
            "modules/entry.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(internal_end, entry_end)),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "intHelper",
            "modules/internal.ts",
            "pkg",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(0, internal_end)),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "app", "modules/app.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(entry_end, app_end)),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(1),
            "pkg",
            "1.0.0",
            "pkg",
        ));
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(2),
            "pkg",
            "1.0.0",
            "pkg/internal/helper.js",
        ));
    rows.package_surfaces.push(PackageSurfaceInput {
        package_name: "pkg".to_string(),
        package_version: Some("1.0.0".to_string()),
        export_specifier: "pkg".to_string(),
        status: PackageAttributionStatus::Accepted,
        evidence: None,
    });

    let enriched = enriched_from_rows(rows);
    let accepted = accepted_external_module_ids(&enriched.model().input().package_attributions);
    let source_facts = super::SourceModuleFacts::from_program(&enriched);

    // Treated as bare-importable (not source-preserved): the internal is freed.
    let freed = super::external_adapters::adapter_required_package_modules(
        &enriched,
        &accepted,
        &source_facts,
        &BTreeSet::new(),
    );
    assert!(
        !freed.contains(&ModuleId(2)),
        "a bare-imported public entry frees its internal",
    );

    // Once the entry is known source-preserved, its kept source re-pins the
    // internal it reads — no phantom binding can survive.
    let mut source_preserved = BTreeSet::new();
    source_preserved.insert(ModuleId(1));
    let repinned = super::external_adapters::adapter_required_package_modules(
        &enriched,
        &accepted,
        &source_facts,
        &source_preserved,
    );
    assert!(
        repinned.contains(&ModuleId(2)),
        "a source-preserved public entry re-pins the internal it reads",
    );
}

#[test]
fn package_only_runtime_helper_inlines_into_single_consumer() {
    let prelude = "function packageHelper() { return 1; }\n";
    let package_body = "var value = packageHelper();\nexport { value };\n";
    let source = format!("{prelude}{package_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(1),
            "package",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    rows.package_attributions
        .push(PackageAttributionInput::rejected_source(
            ModuleId(1),
            "fixture-package",
            "fixture package stays source-backed in planner fixture",
        ));

    let plan = plan_from_rows(rows);
    let package_source = planned_source(&plan, "modules/package.ts");

    assert!(
        package_source.contains("function packageHelper() { return 1; }"),
        "{package_source}"
    );
    assert!(
        !package_source.contains("package-runtime"),
        "{package_source}"
    );
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "central runtime helper should not be emitted for package-only helper"
    );
    assert!(
        planned_source_opt(
            &plan,
            "modules/package-runtime/fixture-package-1.0.0/source-1-helpers.ts"
        )
        .is_none(),
        "single-consumer package helper should be inlined instead of emitted as a helper file"
    );
}

#[test]
fn package_runtime_single_consumer_inlining_eliminates_setter_functions() {
    let prelude = "function seed() { return 0; }\nvar shared = seed();\n";
    let package_body = "shared = 1;\nvar value = shared;\nexport { value };\n";
    let source = format!("{prelude}{package_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(1),
            "package",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    rows.package_attributions
        .push(PackageAttributionInput::rejected_source(
            ModuleId(1),
            "fixture-package",
            "fixture package stays source-backed in planner fixture",
        ));

    let plan = plan_from_rows(rows);
    let package_source = planned_source(&plan, "modules/package.ts");

    assert!(package_source.contains("function seed() { return 0; }"));
    assert!(package_source.contains("var shared = seed();"));
    assert!(package_source.contains("(shared = 1);"), "{package_source}");
    assert!(!package_source.contains("__reverts_set_shared"));
    assert!(
        planned_source_opt(
            &plan,
            "modules/package-runtime/fixture-package-1.0.0/source-1-helpers.ts"
        )
        .is_none()
    );
}

#[test]
fn package_runtime_multi_consumer_keeps_package_runtime_island() {
    let prelude = "function sharedHelper() { return 1; }\n";
    let left_body = "var leftValue = sharedHelper();\nexport { leftValue };\n";
    let right_body = "var rightValue = sharedHelper();\nexport { rightValue };\n";
    let source = format!("{prelude}{left_body}{right_body}");
    let left_start = prelude.len() as u32;
    let left_end = left_start + left_body.len() as u32;
    let right_end = source.len() as u32;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(1),
            "left",
            "modules/left.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(left_start, left_end)),
    );
    rows.modules.push(
        ModuleInput::package(
            ModuleId(2),
            "right",
            "modules/right.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(left_end, right_end)),
    );
    for module_id in [ModuleId(1), ModuleId(2)] {
        rows.package_attributions
            .push(PackageAttributionInput::rejected_source(
                module_id,
                "fixture-package",
                "fixture package stays source-backed in planner fixture",
            ));
    }

    let plan = plan_from_rows(rows);
    let left_source = planned_source(&plan, "modules/left.ts");
    let right_source = planned_source(&plan, "modules/right.ts");
    let island_source = planned_source(
        &plan,
        "modules/package-runtime/fixture-package-1.0.0/source-1-helpers.ts",
    );

    assert!(
        left_source.contains(
            "import { sharedHelper } from './package-runtime/fixture-package-1.0.0/source-1-helpers.js';"
        ),
        "{left_source}"
    );
    assert!(
        right_source.contains(
            "import { sharedHelper } from './package-runtime/fixture-package-1.0.0/source-1-helpers.js';"
        ),
        "{right_source}"
    );
    assert!(island_source.contains("function sharedHelper() { return 1; }"));
    assert!(island_source.contains("export { sharedHelper };"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn app_runtime_consumer_blocks_package_runtime_island() {
    let prelude = "function sharedHelper() { return 1; }\n";
    let package_body = "var packageValue = sharedHelper();\nexport { packageValue };\n";
    let app_body = "var appValue = sharedHelper();\nexport { appValue };\n";
    let source = format!("{prelude}{package_body}{app_body}");
    let package_start = prelude.len() as u32;
    let package_end = package_start + package_body.len() as u32;
    let app_end = source.len() as u32;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(1),
            "package",
            "modules/package.ts",
            "fixture-package",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(package_start, package_end)),
    );
    rows.package_attributions
        .push(PackageAttributionInput::rejected_source(
            ModuleId(1),
            "fixture-package",
            "fixture package stays source-backed in planner fixture",
        ));
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(package_end, app_end)),
    );

    let plan = plan_from_rows(rows);
    let package_source = planned_source(&plan, "modules/package.ts");
    let entry_source = planned_source(&plan, "modules/entry.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        package_source.contains("import { sharedHelper } from './runtime/source-1-helpers.js';"),
        "{package_source}"
    );
    assert!(
        entry_source.contains("import { sharedHelper } from './runtime/source-1-helpers.js';"),
        "{entry_source}"
    );
    assert!(
        helper_source.contains("function sharedHelper() { return 1; }"),
        "{helper_source}"
    );
    assert!(
        planned_source_opt(
            &plan,
            "modules/package-runtime/fixture-package-1.0.0/source-1-helpers.ts"
        )
        .is_none(),
        "application use must keep shared helper in central runtime"
    );
}

#[test]
fn singleton_runtime_function_inlines_into_application_consumer() {
    let prelude = "function runtimeHelper() { return 1; }\n";
    let app_body = "var value = runtimeHelper();\nexport { value };\n";
    let source = format!("{prelude}{app_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(
        entry_source.contains("function runtimeHelper() { return 1; }"),
        "{entry_source}"
    );
    assert!(!entry_source.contains("source-1-helpers"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn singleton_runtime_function_with_runtime_dep_cluster_inlines() {
    let prelude = concat!(
        "function runtimeDep() { return 1; }\n",
        "function runtimeHelper() { return runtimeDep(); }\n",
    );
    let app_body = "var value = runtimeHelper();\nexport { value };\n";
    let source = format!("{prelude}{app_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(entry_source.contains("function runtimeDep() { return 1; }"));
    assert!(entry_source.contains("function runtimeHelper() { return runtimeDep(); }"));
    assert!(!entry_source.contains("source-1-helpers"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn singleton_runtime_function_with_shared_runtime_dep_stays_in_runtime() {
    let prelude = concat!(
        "function runtimeDep() { return 1; }\n",
        "function runtimeHelper() { return runtimeDep(); }\n",
    );
    let helper_body = "var value = runtimeHelper();\nexport { value };\n";
    let dep_body = "var depValue = runtimeDep();\nexport { depValue };\n";
    let source = format!("{prelude}{helper_body}{dep_body}");
    let helper_start = prelude.len() as u32;
    let helper_end = helper_start + helper_body.len() as u32;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "helper-consumer", "modules/helper-consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(helper_start, helper_end)),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "dep-consumer", "modules/dep-consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(helper_end, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let helper_consumer_source = planned_source(&plan, "modules/helper-consumer.ts");
    let dep_consumer_source = planned_source(&plan, "modules/dep-consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        helper_consumer_source
            .contains("import { runtimeHelper } from './runtime/source-1-helpers.js';"),
        "{helper_consumer_source}"
    );
    assert!(
        dep_consumer_source.contains("import { runtimeDep } from './runtime/source-1-helpers.js';"),
        "{dep_consumer_source}"
    );
    assert!(helper_source.contains("function runtimeDep() { return 1; }"));
    assert!(helper_source.contains("function runtimeHelper() { return runtimeDep(); }"));
}

#[test]
fn singleton_runtime_literal_var_inlines_into_application_consumer() {
    let prelude = "var answer = 42;\n";
    let app_body = "var value = answer + 1;\nexport { value };\n";
    let source = format!("{prelude}{app_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(entry_source.contains("var answer = 42;"), "{entry_source}");
    assert!(entry_source.contains("var value = answer + 1;"));
    assert!(!entry_source.contains("source-1-helpers"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn singleton_runtime_pure_object_var_inlines_into_application_consumer() {
    let prelude = "var config = { port: 8080, flags: [true, 'x'] };\n";
    let app_body = "var value = config.flags[0] ? config.port : 0;\nexport { value };\n";
    let source = format!("{prelude}{app_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(
        entry_source.contains("var config = { port: 8080, flags: [true, 'x'] };"),
        "{entry_source}"
    );
    assert!(entry_source.contains("var value = config.flags[0] ? config.port : 0;"));
    assert!(!entry_source.contains("source-1-helpers"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn singleton_runtime_impure_var_stays_in_runtime() {
    let prelude = "var config = buildConfig();\n";
    let app_body = "var value = config.port;\nexport { value };\n";
    let source = format!("{prelude}{app_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let entry_source = planned_source(&plan, "modules/entry.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        entry_source.contains("import { config } from './runtime/source-1-helpers.js';"),
        "{entry_source}"
    );
    assert!(helper_source.contains("var config = buildConfig();"));
    assert!(helper_source.contains("export { config };"));
}

#[test]
fn singleton_runtime_function_with_two_consumers_stays_in_runtime() {
    let prelude = "function runtimeHelper() { return 1; }\n";
    let left_body = "var left = runtimeHelper();\nexport { left };\n";
    let right_body = "var right = runtimeHelper();\nexport { right };\n";
    let source = format!("{prelude}{left_body}{right_body}");
    let left_start = prelude.len() as u32;
    let left_end = left_start + left_body.len() as u32;
    let right_end = source.len() as u32;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "left", "modules/left.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(left_start, left_end)),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "right", "modules/right.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(left_end, right_end)),
    );

    let plan = plan_from_rows(rows);
    let left_source = planned_source(&plan, "modules/left.ts");
    let right_source = planned_source(&plan, "modules/right.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(left_source.contains("import { runtimeHelper } from './runtime/source-1-helpers.js';"));
    assert!(
        right_source.contains("import { runtimeHelper } from './runtime/source-1-helpers.js';")
    );
    assert!(helper_source.contains("function runtimeHelper() { return 1; }"));
}

#[test]
fn source_module_import_takes_precedence_over_same_named_runtime_helper() {
    let planner = ImportExportPlanner;
    let prelude = "function shared() { return 0; }\n";
    let body = "var value = shared();\nexport { value };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.source_files.push(SourceFileInput::new(
        2,
        "shared.js",
        Some("function shared() { return 2; }\n".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "shared", "modules/shared.ts").with_source_file(2),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let entry_source = entry_file.body.join("\n");

    assert!(entry_source.contains("import { shared } from './shared.js';"));
    assert!(!entry_source.contains("source-1-prelude"));
    assert!(!entry_source.contains("source-1-helpers"));
    assert!(
        plan.files
            .iter()
            .all(|file| file.path != "modules/runtime/source-1-helpers.ts")
    );
}

#[test]
fn single_reader_runtime_var_migration_moves_reader_with_writer() {
    let prelude = concat!(
        "var shared;\n",
        "var suffix = '!';\n",
        "function formatShared() { return shared + suffix; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = formatShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(writer_source.contains("import { suffix } from './runtime/source-1-helpers.js';"));
    assert!(writer_source.contains("var shared;"));
    assert!(writer_source.contains("function formatShared() { return shared + suffix; }"));
    assert!(writer_source.contains("shared = 'ok';"));
    assert!(writer_source.contains("export { formatShared };"));
    assert!(!writer_source.contains("__reverts_set_shared"));
    assert!(consumer_source.contains("import { formatShared } from './writer.js';"));
    assert!(!consumer_source.contains("source-1-helpers"));
    assert!(helper_source.contains("var suffix = '!';"));
    assert!(!helper_source.contains("var shared;"));
    assert!(!helper_source.contains("function formatShared()"));
    assert!(!helper_source.contains("__reverts_set_shared"));
    assert!(helper_source.contains("export { suffix };"));
    assert!(!helper_source.contains("from '../writer.js';"));
}

#[test]
fn single_reader_runtime_var_migration_aliases_conflicting_runtime_dep() {
    let prelude = concat!(
        "var shared;\n",
        "var suffix = '!';\n",
        "function formatShared() { return shared + suffix; }\n",
    );
    let writer_body = "var suffix = '?';\nshared = 'ok';\nexport { shared, suffix };\n";
    let consumer_body = "var value = formatShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(writer_source.contains(
        "import { suffix as __reverts_runtime_suffix } from './runtime/source-1-helpers.js';"
    ));
    assert!(
        writer_source
            .contains("function formatShared() { return shared + __reverts_runtime_suffix; }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var suffix = '?';"));
    assert!(writer_source.contains("shared = 'ok';"));
    assert!(!writer_source.contains("__reverts_set_shared"));
    assert!(consumer_source.contains("import { formatShared } from './writer.js';"));
    assert!(helper_source.contains("var suffix = '!';"));
    assert!(!helper_source.contains("var shared;"));
    assert!(!helper_source.contains("function formatShared()"));
    assert!(!helper_source.contains("__reverts_set_shared"));
}

#[test]
fn reader_cluster_runtime_var_migration_migrates_aliased_cross_writer_dep() {
    let prelude = concat!(
        "var shared;\n",
        "var suffix;\n",
        "function formatShared() { return shared + suffix; }\n",
    );
    let writer_body = "var suffix = '?';\nshared = 'ok';\nexport { shared, suffix };\n";
    let suffix_writer_body = "suffix = '!';\nexport { suffix };\n";
    let consumer_body = "var value = formatShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{suffix_writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "suffix-writer", "modules/suffix-writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + suffix_writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + suffix_writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let suffix_writer_source = planned_source(&plan, "modules/suffix-writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        writer_source
            .contains("import { suffix as __reverts_runtime_suffix } from './suffix-writer.js';"),
        "{writer_source}"
    );
    assert!(
        writer_source
            .contains("function formatShared() { return shared + __reverts_runtime_suffix; }"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("var suffix = '?';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(
        suffix_writer_source.contains("var suffix;"),
        "{suffix_writer_source}"
    );
    assert!(
        suffix_writer_source.contains("suffix = '!';"),
        "{suffix_writer_source}"
    );
    assert!(suffix_writer_source.contains("export { suffix };"));
    assert!(consumer_source.contains("import { formatShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn single_reader_runtime_var_migration_handles_reader_without_runtime_deps() {
    let prelude = "var shared;\nfunction getShared() { return shared; }\n";
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = getShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(!writer_source.contains("source-1-helpers"));
    assert!(writer_source.contains("var shared;"));
    assert!(writer_source.contains("function getShared() { return shared; }"));
    assert!(writer_source.contains("shared = 1;"));
    assert!(writer_source.contains("export { getShared };"));
    assert!(consumer_source.contains("import { getShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_moves_private_function_dependency_closure() {
    let prelude = concat!(
        "var shared;\n",
        "function decorate(value) { return value + '!'; }\n",
        "function readShared() { return decorate(shared); }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source.contains("function decorate(value) { return value + '!'; }"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("function readShared() { return decorate(shared); }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(writer_source.contains("export { decorate, readShared };"));
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_moves_private_recursive_function_scc() {
    let prelude = concat!(
        "var shared;\n",
        "function isEven(value) { return value === 0 || isOdd(value - 1); }\n",
        "function isOdd(value) { return value !== 0 && isEven(value - 1); }\n",
        "function readShared() { return isEven(shared); }\n",
    );
    let writer_body = "shared = 4;\nexport { shared };\n";
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("function isEven(value)"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("function isOdd(value)"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("function readShared() { return isEven(shared); }"),
        "{writer_source}"
    );
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_keeps_shared_function_dependency_in_runtime() {
    let prelude = concat!(
        "var shared;\n",
        "function decorate(value) { return value + '!'; }\n",
        "function readShared() { return decorate(shared); }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let other_body = "var other = decorate('x');\nexport { other };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}{other_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + consumer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "other", "modules/other.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + consumer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let other_source = planned_source(&plan, "modules/other.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        writer_source.contains("import { decorate } from './runtime/source-1-helpers.js';"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("function readShared() { return decorate(shared); }"),
        "{writer_source}"
    );
    assert!(
        !writer_source.contains("function decorate(value)"),
        "{writer_source}"
    );
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(other_source.contains("import { decorate } from './runtime/source-1-helpers.js';"));
    assert!(
        helper_source.contains("function decorate(value) { return value + '!'; }"),
        "{helper_source}"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_keeps_writing_function_dependency_in_runtime() {
    let prelude = concat!(
        "var shared;\n",
        "var side;\n",
        "function decorate(value) { side = value; return value; }\n",
        "function readShared() { return decorate(shared); }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        writer_source.contains("import { decorate } from './runtime/source-1-helpers.js';"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("function readShared() { return decorate(shared); }"),
        "{writer_source}"
    );
    assert!(
        !writer_source.contains("function decorate(value)"),
        "{writer_source}"
    );
    assert!(helper_source.contains("var side;"), "{helper_source}");
    assert!(
        helper_source.contains("function decorate(value) { side = value; return value; }"),
        "{helper_source}"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_localizes_leftover_runtime_write() {
    let prelude = concat!(
        "var shared;\n",
        "var side;\n",
        "function readShared() { side = shared; return shared; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("var shared, side;")
            || writer_source.contains("var shared;") && writer_source.contains("var side;"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("function readShared() { side = shared; return shared; }"),
        "{writer_source}"
    );
    assert!(
        !writer_source.contains("__reverts_set_shared"),
        "{writer_source}"
    );
    assert!(
        !writer_source.contains("__reverts_set_side"),
        "{writer_source}"
    );
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "runtime helper should be fully eliminated"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_localizes_setter_dep_with_other_source_reader() {
    let prelude = concat!(
        "var shared;\n",
        "var side;\n",
        "function readShared() { side = shared; return shared; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let observer_body = "var observed = side;\nexport { observed };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}{observer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + consumer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "observer", "modules/observer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + consumer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let observer_source = planned_source(&plan, "modules/observer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("var shared, side;")
            || writer_source.contains("var shared;") && writer_source.contains("var side;"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("function readShared() { side = shared; return shared; }"),
        "{writer_source}"
    );
    assert!(
        !writer_source.contains("__reverts_set_side"),
        "{writer_source}"
    );
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(
        observer_source.contains("import { side } from './writer.js';"),
        "{observer_source}"
    );
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "runtime helper should be fully eliminated"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_localizes_nested_leftover_runtime_write() {
    let prelude = concat!(
        "var shared;\n",
        "var side;\n",
        "function readShared() { side = (() => { side = undefined; return shared; })(); return shared; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("var shared, side;")
            || writer_source.contains("var shared;") && writer_source.contains("var side;"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains(
            "function readShared() { side = (() => { side = undefined; return shared; })(); return shared; }"
        ),
        "{writer_source}"
    );
    assert!(
        !writer_source.contains("__reverts_set_shared"),
        "{writer_source}"
    );
    assert!(
        !writer_source.contains("__reverts_set_side"),
        "{writer_source}"
    );
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "runtime helper should be fully eliminated"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_keeps_setter_dep_with_remaining_runtime_writer() {
    let prelude = concat!(
        "var shared;\n",
        "var side;\n",
        "function readShared() { side = shared; return shared; }\n",
        "function resetSide() { side = 0; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let reset_consumer_body = "var reset = resetSide;\nexport { reset };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}{reset_consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + consumer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "reset-consumer", "modules/reset-consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + consumer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let reset_consumer_source = planned_source(&plan, "modules/reset-consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        writer_source
            .contains("import { __reverts_set_side } from './runtime/source-1-helpers.js';"),
        "{writer_source}"
    );
    assert!(
        writer_source
            .contains("function readShared() { __reverts_set_side(shared); return shared; }"),
        "{writer_source}"
    );
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(
        reset_consumer_source
            .contains("import { resetSide } from './runtime/source-1-helpers.js';"),
        "{reset_consumer_source}"
    );
    assert!(helper_source.contains("var side;"), "{helper_source}");
    assert!(
        helper_source.contains("function resetSide() { side = 0; }"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("function __reverts_set_side(value) { return side = value; }"),
        "{helper_source}"
    );
    assert!(
        !helper_source.contains("function readShared"),
        "{helper_source}"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_treats_runtime_object_mutation_as_read_dependency() {
    let prelude = concat!(
        "var shared;\n",
        "var scratch = [];\n",
        "function readShared() { scratch.length = 0; scratch.push(shared); return shared; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        writer_source.contains("import { scratch } from './runtime/source-1-helpers.js';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source.contains(
            "function readShared() { scratch.length = 0; scratch.push(shared); return shared; }"
        ),
        "{writer_source}"
    );
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(
        helper_source.contains("var scratch = [];"),
        "{helper_source}"
    );
    assert!(!helper_source.contains("var shared;"), "{helper_source}");
    assert!(
        !helper_source.contains("function readShared"),
        "{helper_source}"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_allows_folded_reader_with_bare_setter_dep_cycle() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var side;\n",
        "function readShared() { side = shared; return shared; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let folded_body = concat!(
        "var initUse = lazy(() => { side = readShared(); });\n",
        "export { initUse, side };\n",
    );
    let consumer_body = "var value = initUse();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        writer_source
            .contains("import { __reverts_set_side } from './runtime/source-1-helpers.js';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source
            .contains("function readShared() { __reverts_set_side(shared); return shared; }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        !writer_source.contains("__reverts_set_shared"),
        "{writer_source}"
    );
    assert!(
        helper_source.contains("import { readShared } from '../writer.js';"),
        "{helper_source}"
    );
    assert!(!helper_source.contains("var shared;"), "{helper_source}");
    assert!(
        !helper_source.contains("function readShared()"),
        "{helper_source}"
    );
    assert!(helper_source.contains("var side;"), "{helper_source}");
    assert!(
        helper_source.contains("function __reverts_set_side(value) { return side = value; }"),
        "{helper_source}"
    );
    assert!(
        !helper_source.contains("function __reverts_set_shared"),
        "{helper_source}"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_rejects_folded_reader_with_initialized_setter_dep_cycle() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var side = 'init';\n",
        "function readShared() { side = shared; return shared; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let folded_body = concat!(
        "var initUse = lazy(() => { side = readShared(); });\n",
        "export { initUse, side };\n",
    );
    let consumer_body = "var value = initUse();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(writer_source.contains("shared = 'ok';"));
    assert!(
        !writer_source.contains("function readShared()"),
        "{writer_source}"
    );
    assert!(
        helper_source.contains("var side = 'init';"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("function readShared() { side = shared; return shared; }"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("import { shared } from '../writer.js';"),
        "{helper_source}"
    );
    assert!(
        !helper_source.contains("function __reverts_set_shared"),
        "{helper_source}"
    );
}

#[test]
fn runtime_var_migration_preserves_pure_object_initializer_in_writer() {
    let prelude = "var shared = { ready: false, tags: ['a'] };\n";
    let writer_body = "shared = { ready: true, tags: ['b'] };\nexport { shared };\n";
    let consumer_body = "var value = shared.ready;\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        writer_source.contains("var shared = { ready: false, tags: ['a'] };"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("shared = {\n\tready: true,\n\ttags: ['b']\n};"),
        "{writer_source}"
    );
    assert!(!writer_source.contains("__reverts_set_shared"));
    assert!(consumer_source.contains("import { shared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_moves_arrow_reader_with_writer() {
    let prelude = "var shared;\nvar getShared = () => shared;\n";
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = getShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(writer_source.contains("var getShared = () => shared;"));
    assert!(writer_source.contains("shared = 1;"));
    assert!(writer_source.contains("export { getShared };"));
    assert!(consumer_source.contains("import { getShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_moves_generator_reader_with_writer() {
    let prelude = "var shared;\nfunction* streamShared() { yield shared; }\n";
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = streamShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(writer_source.contains("function* streamShared() { yield shared; }"));
    assert!(writer_source.contains("shared = 1;"));
    assert!(writer_source.contains("export { streamShared };"));
    assert!(consumer_source.contains("import { streamShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_moves_async_generator_reader_with_writer() {
    let prelude = "var shared;\nasync function* streamShared() { yield shared; }\n";
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = streamShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(writer_source.contains("async function* streamShared() { yield shared; }"));
    assert!(writer_source.contains("shared = 1;"));
    assert!(writer_source.contains("export { streamShared };"));
    assert!(consumer_source.contains("import { streamShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_moves_class_reader_with_writer() {
    let prelude = concat!(
        "var shared;\n",
        "class ReadsShared { value() { return shared; } }\n",
    );
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = new ReadsShared().value();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(writer_source.contains("class ReadsShared { value() { return shared; } }"));
    assert!(writer_source.contains("shared = 1;"));
    assert!(writer_source.contains("export { ReadsShared };"));
    assert!(consumer_source.contains("import { ReadsShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_allows_instance_field_array_initializer() {
    let prelude = concat!(
        "var shared;\n",
        "class ReadsShared { cache = []; value() { return shared; } }\n",
    );
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = new ReadsShared().value();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("class ReadsShared"),
        "{writer_source}"
    );
    assert!(writer_source.contains("cache = [];"), "{writer_source}");
    assert!(
        writer_source.contains("value() { return shared; }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 1;"));
    assert!(writer_source.contains("export { ReadsShared };"));
    assert!(consumer_source.contains("import { ReadsShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_moves_static_method_class_reader_with_writer() {
    let prelude = concat!(
        "var shared;\n",
        "class ReadsShared { static ready = false; static value() { return shared; } }\n",
    );
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = ReadsShared.value();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source.contains(
            "class ReadsShared { static ready = false; static value() { return shared; } }"
        ),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 1;"));
    assert!(writer_source.contains("export { ReadsShared };"));
    assert!(consumer_source.contains("import { ReadsShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_rejects_static_class_reader() {
    let prelude = concat!(
        "var shared;\n",
        "class ReadsShared { static value = shared; }\n",
    );
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = ReadsShared.value;\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(writer_source.contains("__reverts_set_shared(1);"));
    assert!(!writer_source.contains("class ReadsShared"));
    assert!(helper_source.contains("class ReadsShared { static value = shared; }"));
    assert!(
        helper_source.contains("function __reverts_set_shared(value) { return shared = value; }")
    );
}

#[test]
fn reader_cluster_runtime_var_migration_rejects_static_block_class_reader() {
    let prelude = concat!(
        "var shared;\n",
        "class ReadsShared { static { this.value = shared; } }\n",
    );
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = ReadsShared.value;\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(writer_source.contains("__reverts_set_shared(1);"));
    assert!(!writer_source.contains("class ReadsShared"));
    assert!(helper_source.contains("class ReadsShared { static { this.value = shared; } }"));
    assert!(
        helper_source.contains("function __reverts_set_shared(value) { return shared = value; }")
    );
}

#[test]
fn reader_cluster_runtime_var_migration_rejects_computed_class_reader_key() {
    let prelude = concat!(
        "var shared;\n",
        "class ReadsShared { [shared]() { return 1; } }\n",
    );
    let writer_body = "shared = 'value';\nexport { shared };\n";
    let consumer_body = "var value = new ReadsShared()[shared]();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(writer_source.contains("__reverts_set_shared('value');"));
    assert!(!writer_source.contains("class ReadsShared"));
    assert!(helper_source.contains("class ReadsShared { [shared]() { return 1; } }"));
    assert!(
        helper_source.contains("function __reverts_set_shared(value) { return shared = value; }")
    );
}

#[test]
fn reader_cluster_runtime_var_migration_moves_multiple_readers() {
    let prelude = concat!(
        "var shared;\n",
        "function first() { return shared; }\n",
        "function second() { return shared; }\n",
    );
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = first();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(!writer_source.contains("source-1-helpers"));
    assert!(writer_source.contains("var shared;"));
    assert!(writer_source.contains("function first() { return shared; }"));
    assert!(writer_source.contains("function second() { return shared; }"));
    assert!(writer_source.contains("shared = 1;"));
    assert!(writer_source.contains("first"));
    assert!(writer_source.contains("second"));
    assert!(consumer_source.contains("import { first } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_moves_dependent_reader_chain() {
    let prelude = concat!(
        "var shared;\n",
        "function rawShared() { return shared; }\n",
        "function decoratedShared() { return rawShared() + '!'; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = decoratedShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(writer_source.contains("function rawShared() { return shared; }"));
    assert!(writer_source.contains("function decoratedShared() { return rawShared() + '!'; }"));
    assert!(writer_source.contains("decoratedShared"));
    assert!(writer_source.contains("rawShared"));
    assert!(consumer_source.contains("import { decoratedShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_moves_reader_that_writes_primary() {
    let prelude = concat!(
        "var shared;\n",
        "function resetShared() { shared = 0; return shared; }\n",
    );
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = resetShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(writer_source.contains("function resetShared() { shared = 0; return shared; }"));
    assert!(writer_source.contains("shared = 1;"));
    assert!(writer_source.contains("export { resetShared };"));
    assert!(consumer_source.contains("import { resetShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_ignores_local_write_shadowing_runtime_name() {
    let prelude = concat!(
        "var shared;\n",
        "var cache;\n",
        "function readShared() { let cache = 0; cache = shared; return cache; }\n",
    );
    let writer_body = "shared = 1;\nexport { shared };\n";
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source
            .contains("function readShared() { let cache = 0; cache = shared; return cache; }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 1;"), "{writer_source}");
    assert!(writer_source.contains("export { readShared };"));
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_allows_owner_local_source_dependency() {
    let prelude = concat!(
        "var shared;\n",
        "function decoratedShared() { return decorate(shared); }\n",
    );
    let writer_body = concat!(
        "function decorate(value) { return `${value}!`; }\n",
        "shared = 'ok';\n",
        "export { shared };\n",
    );
    let consumer_body = "var value = decoratedShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source.contains("function decoratedShared() { return decorate(shared); }"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("function decorate(value)"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("return `${value}!`;"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(writer_source.contains("export { decoratedShared };"));
    assert!(consumer_source.contains("import { decoratedShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_imports_declared_source_dependency() {
    let prelude = concat!(
        "var shared;\n",
        "function decoratedShared() { return decorate(shared); }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let helper_body = "function decorate(value) { return `${value}!`; }\n";
    let consumer_body = "var value = decoratedShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{helper_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "helper", "modules/helper.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + helper_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + helper_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/helper.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        writer_source.contains("import { decorate } from './helper.js';"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("function decoratedShared() { return decorate(shared); }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        helper_source.contains("export { decorate };"),
        "{helper_source}"
    );
    assert!(consumer_source.contains("import { decoratedShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_imports_auto_source_dependency() {
    let prelude = concat!(
        "var shared;\n",
        "function decoratedShared() { return decorate(shared); }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let helper_body = "function decorate(value) { return `${value}!`; }\n";
    let consumer_body = "var value = decoratedShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{helper_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "helper", "modules/helper.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + helper_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + helper_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/helper.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        writer_source.contains("import { decorate } from './helper.js';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("function decoratedShared()"));
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        helper_source.contains("export { decorate };"),
        "{helper_source}"
    );
    assert!(consumer_source.contains("import { decoratedShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_imports_implicit_source_write_dependency() {
    let prelude = concat!(
        "var shared;\n",
        "function decoratedShared() { return decorate(shared); }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let helper_body = "decorate = (value) => `${value}!`;\n";
    let consumer_body = "var value = decoratedShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{helper_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "helper", "modules/helper.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + helper_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + helper_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/helper.ts");

    assert!(
        writer_source.contains("import { decorate } from './helper.js';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("function decoratedShared()"));
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(helper_source.contains("var decorate;"), "{helper_source}");
    assert!(
        helper_source.contains("decorate = (value) => `${value}!`;"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("export { decorate };"),
        "{helper_source}"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_erases_externalized_init_shim_locally() {
    let prelude = "var shared;\nfunction readShared() { cNq(); return shared; }\n";
    let package_body = "function cNq() {}\nexport { cNq };\n";
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = readShared();\nexport { value };\n";
    let source = format!("{prelude}{package_body}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::package(
            ModuleId(1),
            "open",
            "modules/open.ts",
            "open",
            Some("1.0.0".to_string()),
        )
        .with_source_file(1)
        .with_source_span(SourceSpan::new(
            prelude.len() as u32,
            (prelude.len() + package_body.len()) as u32,
        )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + package_body.len()) as u32,
                (prelude.len() + package_body.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + package_body.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.package_attributions
        .push(PackageAttributionInput::accepted_external(
            ModuleId(1),
            "open",
            "1.0.0",
            "open/index.js",
        ));

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(writer_source.contains("return shared;"), "{writer_source}");
    assert!(!writer_source.contains("function cNq()"), "{writer_source}");
    assert!(!writer_source.contains("cNq();"), "{writer_source}");
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(consumer_source.contains("import { readShared } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn namespace_getter_runtime_var_migration_moves_namespace_with_writer() {
    let prelude = concat!(
        "var shared;\n",
        "var ns = {};\n",
        "function expose(target, exports) {}\n",
        "expose(ns, { value: () => shared });\n",
        "function readShared() { return ns.value; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = readShared() + ns.value;\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(!writer_source.contains("source-1-helpers"));
    assert!(writer_source.contains("var shared;"));
    assert!(writer_source.contains("var ns = {};"));
    assert!(writer_source.contains(
        "Object.defineProperties(ns, { value: { enumerable: true, get: () => shared } });"
    ));
    assert!(writer_source.contains("function readShared() { return ns.value; }"));
    assert!(writer_source.contains("shared = 'ok';"));
    assert!(writer_source.contains("export { ns, readShared };"));
    assert!(
        consumer_source.contains("import { readShared, shared } from './writer.js';"),
        "{consumer_source}"
    );
    assert!(consumer_source.contains("var value = readShared() + shared;"));
    assert!(!consumer_source.contains("ns.value"));
    let helper_source = planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts");
    assert!(
        helper_source.is_none(),
        "writer:\n{writer_source}\nconsumer:\n{consumer_source}\nhelper:\n{helper_source:?}"
    );
}

#[test]
fn namespace_getter_runtime_var_migration_strips_moved_namespace_when_runtime_remains() {
    let prelude = concat!(
        "var shared;\n",
        "var keep = 1;\n",
        "var ns = {};\n",
        "function expose(target, exports) {}\n",
        "expose(ns, { value: () => shared });\n",
        "function readShared() { return ns.value; }\n",
        "function useKeep() { return keep; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let consumer_body = "var value = readShared() + ns.value + useKeep();\nexport { value };\n";
    let other_consumer_body = "var other = useKeep();\nexport { other };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}{other_consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + consumer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "other-consumer", "modules/other-consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + consumer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(writer_source.contains("var ns = {};"), "{writer_source}");
    assert!(writer_source.contains(
        "Object.defineProperties(ns, { value: { enumerable: true, get: () => shared } });"
    ));
    assert!(
        consumer_source.contains("import { readShared, shared } from './writer.js';"),
        "{consumer_source}"
    );
    assert!(
        consumer_source.contains("import { useKeep } from './runtime/source-1-helpers.js';"),
        "{consumer_source}"
    );
    assert!(consumer_source.contains("readShared() + shared + useKeep()"));
    assert!(
        helper_source.contains("function useKeep()"),
        "{helper_source}"
    );
    assert!(!helper_source.contains("var ns = {};"), "{helper_source}");
    assert!(
        !helper_source.contains("Object.defineProperties(ns"),
        "{helper_source}"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_moves_same_writer_component() {
    let prelude = "var left;\nvar right;\nfunction pair() { return [left, right]; }\n";
    let writer_body = "left = 1;\nright = 2;\nexport { left, right };\n";
    let consumer_body = "var value = pair();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("var left, right;") || writer_source.contains("var left;"),
        "{writer_source}"
    );
    assert!(writer_source.contains("function pair() { return [left, right]; }"));
    assert!(writer_source.contains("left = 1;"));
    assert!(writer_source.contains("right = 2;"));
    assert!(writer_source.contains("export { pair };"));
    assert!(consumer_source.contains("import { pair } from './writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_moves_folded_same_writer_dep_without_cycle() {
    let prelude = "var left;\nvar right;\nvar used;\nfunction pair() { return [left, right]; }\n";
    let writer_body = "left = 1;\nright = 2;\nexport { left, right };\n";
    let folded_body =
        "var useRight = lazyValue(() => { used = right; });\nexport { useRight, used };\n";
    let consumer_body = "var value = pair();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("var left, right;") || writer_source.contains("var left;"),
        "{writer_source}"
    );
    assert!(writer_source.contains("function pair() { return [left, right]; }"));
    assert!(writer_source.contains("left = 1;"));
    assert!(writer_source.contains("right = 2;"));
    assert!(!writer_source.contains("__reverts_set_right"));
    assert!(writer_source.contains("export { pair };"));
    assert!(consumer_source.contains("import { pair } from './writer.js';"));
    assert!(!helper_source.contains("function pair()"));
    assert!(!helper_source.contains("var left;"));
    assert!(!helper_source.contains("var right;"));
    assert!(
        helper_source.contains("import { right } from '../writer.js';"),
        "{helper_source}"
    );
    assert!(helper_source.contains("var used;"), "{helper_source}");
    assert!(
        helper_source.contains("var useRight = lazyValue(() => { used = right; });"),
        "{helper_source}"
    );
    assert!(!helper_source.contains("__reverts_set_right"));
    assert!(!helper_source.contains("__reverts_set_left"));
}

#[test]
fn reader_cluster_runtime_var_migration_imports_cross_writer_movable_dep() {
    let prelude = "var left;\nvar right;\nfunction pair() { return [left, right]; }\n";
    let left_writer_body = "left = 1;\nexport { left };\n";
    let right_writer_body = "right = 2;\nexport { right };\n";
    let consumer_body = "var value = pair();\nexport { value };\n";
    let source = format!("{prelude}{left_writer_body}{right_writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "left-writer", "modules/left-writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + left_writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "right-writer", "modules/right-writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + left_writer_body.len()) as u32,
                (prelude.len() + left_writer_body.len() + right_writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + left_writer_body.len() + right_writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let left_writer_source = planned_source(&plan, "modules/left-writer.ts");
    let right_writer_source = planned_source(&plan, "modules/right-writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        left_writer_source.contains("import { right } from './right-writer.js';"),
        "{left_writer_source}"
    );
    assert!(
        left_writer_source.contains("var left;"),
        "{left_writer_source}"
    );
    assert!(left_writer_source.contains("function pair() { return [left, right]; }"));
    assert!(left_writer_source.contains("left = 1;"));
    assert!(left_writer_source.contains("export { pair };"));
    assert!(right_writer_source.contains("var right;"));
    assert!(right_writer_source.contains("right = 2;"));
    assert!(!right_writer_source.contains("__reverts_set_right"));
    assert!(!right_writer_source.contains("function pair()"));
    assert!(consumer_source.contains("import { pair } from './left-writer.js';"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn reader_cluster_runtime_var_migration_pins_cyclic_cross_writer_dep_in_runtime() {
    let prelude = "var left;\nvar right;\nfunction pair() { return [left, right]; }\n";
    let left_writer_body = "left = 1;\nexport { left };\n";
    let right_writer_body = "right = 2;\nexport { right };\n";
    let consumer_body = "var value = pair();\nexport { value };\n";
    let source = format!("{prelude}{left_writer_body}{right_writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "left-writer", "modules/left-writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + left_writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "right-writer", "modules/right-writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + left_writer_body.len()) as u32,
                (prelude.len() + left_writer_body.len() + right_writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + left_writer_body.len() + right_writer_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });

    let plan = plan_from_rows(rows);
    let left_writer_source = planned_source(&plan, "modules/left-writer.ts");
    let right_writer_source = planned_source(&plan, "modules/right-writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        left_writer_source.contains("import { right } from './runtime/source-1-helpers.js';"),
        "{left_writer_source}"
    );
    assert!(
        left_writer_source.contains("var left;"),
        "{left_writer_source}"
    );
    assert!(left_writer_source.contains("function pair() { return [left, right]; }"));
    assert!(left_writer_source.contains("left = 1;"));
    assert!(left_writer_source.contains("export { pair };"));
    assert!(right_writer_source.contains("__reverts_set_right(2);"));
    assert!(!right_writer_source.contains("function pair()"));
    assert!(consumer_source.contains("import { pair } from './left-writer.js';"));
    assert!(helper_source.contains("var right;"));
    assert!(!helper_source.contains("var left;"));
    assert!(
        helper_source.contains("function __reverts_set_right(value) { return right = value; }")
    );
    assert!(
        !helper_source.contains("function __reverts_set_left"),
        "{helper_source}"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_imports_folded_runtime_local_dep() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var decorate;\n",
        "function decoratedShared() { initDecorate(); return decorate(shared); }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let folded_body = concat!(
        "var initDecorate = lazy(() => { decorate = (value) => `${value}!`; });\n",
        "export { initDecorate, decorate };\n",
    );
    let consumer_body = "var value = decoratedShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "decorator", "modules/decorator.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        writer_source
            .contains("import { decorate, initDecorate } from './runtime/source-1-helpers.js';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source
            .contains("function decoratedShared() { initDecorate(); return decorate(shared); }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(writer_source.contains("export { decoratedShared };"));
    assert!(consumer_source.contains("import { decoratedShared } from './writer.js';"));
    assert!(!helper_source.contains("function decoratedShared()"));
    assert!(!helper_source.contains("var shared;"));
    assert!(
        !helper_source.contains("__reverts_set_shared"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("decorate = (value) => `${value}!`;"),
        "{helper_source}"
    );
    assert!(helper_source.contains("var initDecorate = () => {};"));
    assert!(helper_source.contains("export { decorate, initDecorate };"));
}

#[test]
fn reader_cluster_runtime_var_migration_imports_ambiguous_folded_runtime_dep() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var decorate;\n",
        "function decoratedShared() { initDecorate(); return decorate(shared); }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let folded_body = concat!(
        "var initDecorate = lazy(() => { decorate = (value) => `${value}!`; });\n",
        "export { initDecorate, decorate };\n",
    );
    let duplicate_body = "var decorate = (value) => value;\nexport { decorate };\n";
    let consumer_body = "var value = decoratedShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{duplicate_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "decorator", "modules/decorator.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "duplicate", "modules/duplicate.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len() + duplicate_body.len())
                    as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(4), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len() + duplicate_body.len())
                    as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        writer_source.contains("import { initDecorate } from './runtime/source-1-helpers.js';"),
        "{writer_source}"
    );
    assert!(
        writer_source.contains("import { decorate } from './duplicate.js';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("function decoratedShared()"));
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        !writer_source.contains("__reverts_set_shared"),
        "{writer_source}"
    );
    assert!(!helper_source.contains("var shared;"), "{helper_source}");
    assert!(
        !helper_source.contains("__reverts_set_shared"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("decorate = (value) => `${value}!`;"),
        "{helper_source}"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_allows_folded_reader_use_without_runtime_deps() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var used = Date.now();\n",
        "function readShared() { return shared; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let folded_body = concat!(
        "var initUse = lazy(() => { used = readShared(); });\n",
        "export { initUse, used };\n",
    );
    let consumer_body = "var value = initUse();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source.contains("function readShared() { return shared; }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(writer_source.contains("export { readShared };"));
    assert!(
        helper_source.contains("import { readShared } from '../writer.js';"),
        "{helper_source}"
    );
    assert!(
        !helper_source.contains("function readShared()"),
        "{helper_source}"
    );
    assert!(!helper_source.contains("var shared;"), "{helper_source}");
    assert!(
        !helper_source.contains("__reverts_set_shared"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("used = readShared();"),
        "{helper_source}"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_allows_folded_primary_and_reader_use() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var used = Date.now();\n",
        "function readShared() { return shared; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let folded_body = concat!(
        "var initUse = lazy(() => { used = shared + readShared(); });\n",
        "export { initUse, used };\n",
    );
    let consumer_body = "var value = initUse();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source.contains("function readShared() { return shared; }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        helper_source.contains("import { readShared, shared } from '../writer.js';"),
        "{helper_source}"
    );
    assert!(!helper_source.contains("function readShared()"));
    assert!(!helper_source.contains("var shared;"), "{helper_source}");
    assert!(
        !helper_source.contains("__reverts_set_shared"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("used = shared + readShared();"),
        "{helper_source}"
    );
}

#[test]
fn reader_cluster_runtime_var_migration_allows_folded_reader_use_when_writer_lazy_localizes() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var used = Date.now();\n",
        "function readShared() { return shared; }\n",
    );
    let writer_body = concat!(
        "var writerInit = lazy(() => { shared = 'ok'; });\n",
        "writerInit();\n",
        "export { shared, writerInit };\n",
    );
    let folded_body = concat!(
        "var initUse = lazy(() => { used = readShared(); });\n",
        "export { initUse, used };\n",
    );
    let consumer_body = "var value = initUse();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    // Writer stays local; the global de-lazify post-pass then eagerifies
    // `writerInit` (body `shared = 'ok';`, no top-level return, invoked here) to
    // a no-op stub. `readShared` reads `shared` AFTER its eager init runs at
    // module-eval, so it still observes 'ok'. Dead local `_$l` memoizer dropped.
    assert!(!writer_source.contains("_$l"), "{writer_source}");
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        writer_source.contains("function writerInit() {}"),
        "{writer_source}"
    );
    assert!(!writer_source.contains("lazyValue("), "{writer_source}");
    assert!(
        writer_source.contains("function readShared() { return shared; }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("export { readShared };"));
    assert!(
        helper_source.contains("import { readShared } from '../writer.js';"),
        "{helper_source}"
    );
    assert!(
        !helper_source.contains("function readShared()"),
        "{helper_source}"
    );
    assert!(!helper_source.contains("var shared;"), "{helper_source}");
    assert!(
        !helper_source.contains("__reverts_set_shared"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("used = readShared();"),
        "{helper_source}"
    );
}

#[test]
fn folded_non_snippet_runtime_read_migration_allows_writer_lazy_localization() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var used = Date.now();\n",
    );
    let writer_body = concat!(
        "var writerInit = lazy(() => { shared = 'ok'; });\n",
        "writerInit();\n",
        "export { shared, writerInit };\n",
    );
    let folded_body = concat!(
        "var initUse = lazy(() => { used = shared; });\n",
        "export { initUse, used };\n",
    );
    let consumer_body = "var value = initUse();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        !writer_source.contains("source-1-helpers"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    // Writer localizes (no setter); the global de-lazify post-pass then
    // eagerifies `writerInit` (body `shared = 'ok';`, no top-level return,
    // invoked here) to a no-op stub. Dead local `_$l` memoizer dropped.
    assert!(!writer_source.contains("_$l"), "{writer_source}");
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        writer_source.contains("function writerInit() {}"),
        "{writer_source}"
    );
    assert!(!writer_source.contains("lazyValue("), "{writer_source}");
    assert!(
        helper_source.contains("import { shared } from '../writer.js';"),
        "{helper_source}"
    );
    assert!(!helper_source.contains("var shared;"), "{helper_source}");
    assert!(
        !helper_source.contains("__reverts_set_shared"),
        "{helper_source}"
    );
    assert!(helper_source.contains("used = shared;"), "{helper_source}");
}

#[test]
fn reader_cluster_runtime_var_migration_allows_lazy_folded_reader_use_with_runtime_dep() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var suffix = '!';\n",
        "var used = Date.now();\n",
        "function readShared() { return shared + suffix; }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let folded_body = concat!(
        "var initUse = lazy(() => { used = readShared(); });\n",
        "export { initUse, used };\n",
    );
    let consumer_body = "var value = initUse();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        writer_source.contains("import { suffix } from './runtime/source-1-helpers.js';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source.contains("function readShared() { return shared + suffix; }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        !writer_source.contains("__reverts_set_shared"),
        "{writer_source}"
    );
    assert!(!helper_source.contains("function readShared()"));
    assert!(!helper_source.contains("var shared;"), "{helper_source}");
    assert!(
        !helper_source.contains("__reverts_set_shared"),
        "{helper_source}"
    );
    assert!(helper_source.contains("import { readShared } from '../writer.js';"));
}

#[test]
fn reader_cluster_runtime_var_migration_allows_source_cyclic_folded_runtime_local_dep() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared;\n",
        "var decorate;\n",
        "function decoratedShared() { initDecorate(); return decorate(shared); }\n",
    );
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let folded_body = concat!(
        "var initDecorate = lazy(() => { decorate = (value) => `${value}!`; });\n",
        "export { initDecorate, decorate };\n",
    );
    let consumer_body = "var value = decoratedShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{folded_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "decorator", "modules/decorator.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + folded_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        writer_source
            .contains("import { decorate, initDecorate } from './runtime/source-1-helpers.js';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"), "{writer_source}");
    assert!(
        writer_source
            .contains("function decoratedShared() { initDecorate(); return decorate(shared); }"),
        "{writer_source}"
    );
    assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
    assert!(
        !writer_source.contains("__reverts_set_shared"),
        "{writer_source}"
    );
    assert!(!helper_source.contains("function decoratedShared()"));
    assert!(!helper_source.contains("var shared;"));
    assert!(
        !helper_source.contains("__reverts_set_shared"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("decorate = (value) => `${value}!`;"),
        "{helper_source}"
    );
}

#[test]
fn runtime_setter_migration_blocker_report_counts_gate_reasons_without_changing_gate() {
    let prelude = concat!(
        "var accepted;\n",
        "var left;\n",
        "var right;\n",
        "function pair() { return [left, right]; }\n",
        "var complex = makeValue();\n",
        "var multi;\n",
    );
    let writer_body = concat!(
        "accepted = 1;\n",
        "left = 2;\n",
        "right = 3;\n",
        "complex = 4;\n",
        "multi = 5;\n",
        "export { accepted, left, right, complex, multi };\n",
    );
    let second_writer_body = "multi = 6;\nexport { multi };\n";
    let consumer_body = "var value = pair();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{second_writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "second-writer", "modules/second-writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + second_writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + second_writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let enriched = enriched_from_rows(rows);
    let report = ImportExportPlanner.runtime_setter_migration_blocker_report(&enriched);
    let plan = ImportExportPlanner
        .plan_enriched_program(&enriched)
        .expect("plan should still be valid");
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert_eq!(report.total_bindings, 5);
    assert_eq!(report.accepted_bindings, 3);
    assert_eq!(report.blocked_bindings, 2);
    assert_eq!(
        report
            .reasons
            .get(&RuntimeSetterMigrationBlockerReason::ReaderReadsOtherMovableBinding),
        None
    );
    assert_eq!(
        report
            .reasons
            .get(&RuntimeSetterMigrationBlockerReason::InitializerNotMigratable),
        Some(&1)
    );
    assert_eq!(
        report
            .reasons
            .get(&RuntimeSetterMigrationBlockerReason::MultipleEligibleWriters),
        Some(&1)
    );
    assert!(
        writer_source.contains("var accepted, left, right;")
            || writer_source.contains("var accepted;"),
        "{writer_source}"
    );
    assert!(writer_source.contains("accepted = 1;"), "{writer_source}");
    assert!(writer_source.contains("function pair() { return [left, right]; }"));
    assert!(writer_source.contains("left = 2;"), "{writer_source}");
    assert!(writer_source.contains("right = 3;"), "{writer_source}");
    assert!(
        !helper_source.contains("__reverts_set_left"),
        "{helper_source}"
    );
    assert!(
        !helper_source.contains("__reverts_set_right"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("function __reverts_set_complex(value) { return complex = value; }"),
        "{helper_source}"
    );
    assert!(
        helper_source.contains("function __reverts_set_multi(value) { return multi = value; }"),
        "{helper_source}"
    );
}

#[test]
fn reader_migration_merge_combines_same_owner_overlapping_components() {
    let owner = ModuleId(1);
    let other_owner = ModuleId(2);
    let prelude = RuntimePrelude {
        source_file_id: 1,
        source_file_path: "bundle.js".to_string(),
        source: String::new(),
        bindings: BTreeMap::new(),
        snippets: BTreeMap::from([
            (
                BindingName::new("readA"),
                RuntimePreludeSnippet {
                    source: "function readA() { return a + b; }".to_string(),
                    byte_start: 10,
                    sub_snippets: Vec::new(),
                },
            ),
            (
                BindingName::new("readC"),
                RuntimePreludeSnippet {
                    source: "function readC() { return b + c; }".to_string(),
                    byte_start: 20,
                    sub_snippets: Vec::new(),
                },
            ),
            (
                BindingName::new("readD"),
                RuntimePreludeSnippet {
                    source: "function readD() { return b + d; }".to_string(),
                    byte_start: 30,
                    sub_snippets: Vec::new(),
                },
            ),
        ]),
        namespace_exports: Vec::new(),
        entrypoint: None,
    };
    let source_consumers = BTreeMap::new();
    let source_definition_modules = BTreeMap::new();
    let all_source_definition_modules = BTreeMap::new();
    let externalized_packages = BTreeSet::new();
    let module_dependencies = BTreeMap::new();
    let folded_modules = BTreeSet::new();
    let folded_definitions = BTreeSet::new();
    let owner_available_bindings = BTreeMap::new();
    let owner_state = BTreeMap::new();
    let owner_source_lines = BTreeMap::new();
    let read_index = RuntimeSourceReadIndex::default();
    let movable = BTreeSet::new();
    let candidate_owners = BTreeMap::new();
    let ctx = RuntimeReaderClusterContext {
        source_file_id: 1,
        owner_available_bindings: &owner_available_bindings,
        source_consumers_by_runtime_binding: &source_consumers,
        source_definition_modules: &source_definition_modules,
        all_source_definition_modules: &all_source_definition_modules,
        externalized_packages: &externalized_packages,
        module_dependencies_by_owner: &module_dependencies,
        folded_modules: &folded_modules,
        folded_runtime_definitions: &folded_definitions,
        owner_runtime_state: &owner_state,
        owner_source_lines: &owner_source_lines,
        prelude: &prelude,
        read_index: &read_index,
        movable_bindings: &movable,
        candidate_owners: &candidate_owners,
    };
    let proposals = vec![
        RuntimeReaderClusterMigrationProposal {
            seed_binding: BindingName::new("a"),
            owner_module: owner,
            source_lines: 1,
            migration: RuntimeReaderClusterMigration {
                primary_bindings: BTreeSet::from([BindingName::new("a"), BindingName::new("b")]),
                extra_snippets: BTreeSet::from([BindingName::new("readA")]),
                extra_namespace_exports: BTreeSet::new(),
                extra_runtime_deps: BTreeSet::from([BindingName::new("b")]),
                extra_runtime_setter_deps: BTreeSet::new(),
                extra_runtime_dep_aliases: BTreeMap::new(),
                pinned_runtime_deps: BTreeSet::new(),
                extra_source_deps: BTreeMap::new(),
                extra_runtime_reexport_source_deps: BTreeMap::new(),
                extra_noop_deps: BTreeSet::new(),
            },
        },
        RuntimeReaderClusterMigrationProposal {
            seed_binding: BindingName::new("c"),
            owner_module: owner,
            source_lines: 1,
            migration: RuntimeReaderClusterMigration {
                primary_bindings: BTreeSet::from([BindingName::new("b"), BindingName::new("c")]),
                extra_snippets: BTreeSet::from([BindingName::new("readC")]),
                extra_namespace_exports: BTreeSet::new(),
                extra_runtime_deps: BTreeSet::new(),
                extra_runtime_setter_deps: BTreeSet::new(),
                extra_runtime_dep_aliases: BTreeMap::new(),
                pinned_runtime_deps: BTreeSet::new(),
                extra_source_deps: BTreeMap::new(),
                extra_runtime_reexport_source_deps: BTreeMap::new(),
                extra_noop_deps: BTreeSet::new(),
            },
        },
        RuntimeReaderClusterMigrationProposal {
            seed_binding: BindingName::new("d"),
            owner_module: other_owner,
            source_lines: 1,
            migration: RuntimeReaderClusterMigration {
                primary_bindings: BTreeSet::from([BindingName::new("b"), BindingName::new("d")]),
                extra_snippets: BTreeSet::from([BindingName::new("readD")]),
                extra_namespace_exports: BTreeSet::new(),
                extra_runtime_deps: BTreeSet::new(),
                extra_runtime_setter_deps: BTreeSet::new(),
                extra_runtime_dep_aliases: BTreeMap::new(),
                pinned_runtime_deps: BTreeSet::new(),
                extra_source_deps: BTreeMap::new(),
                extra_runtime_reexport_source_deps: BTreeMap::new(),
                extra_noop_deps: BTreeSet::new(),
            },
        },
    ];

    let merged = merge_same_owner_overlapping_reader_migrations(&ctx, proposals);
    let owner_merge = merged
        .iter()
        .find(|proposal| proposal.owner_module == owner)
        .expect("same-owner overlap should merge");

    assert_eq!(merged.len(), 2);
    assert_eq!(
        owner_merge.migration.primary_bindings,
        BTreeSet::from([
            BindingName::new("a"),
            BindingName::new("b"),
            BindingName::new("c"),
        ])
    );
    assert_eq!(
        owner_merge.migration.extra_snippets,
        BTreeSet::from([BindingName::new("readA"), BindingName::new("readC")])
    );
    assert!(owner_merge.migration.extra_runtime_deps.is_empty());
    assert!(
        merged
            .iter()
            .any(|proposal| proposal.owner_module == other_owner)
    );
}

#[test]
fn reader_migration_conflict_selector_prefers_total_primary_coverage() {
    let proposal = |seed: &str, owner: ModuleId, primary_bindings: &[&str], source_lines: usize| {
        RuntimeReaderClusterMigrationProposal {
            seed_binding: BindingName::new(seed),
            owner_module: owner,
            source_lines,
            migration: RuntimeReaderClusterMigration {
                primary_bindings: primary_bindings
                    .iter()
                    .map(|binding| BindingName::new(*binding))
                    .collect(),
                extra_snippets: BTreeSet::from([BindingName::new(format!("read_{seed}"))]),
                extra_namespace_exports: BTreeSet::new(),
                extra_runtime_deps: BTreeSet::new(),
                extra_runtime_setter_deps: BTreeSet::new(),
                extra_runtime_dep_aliases: BTreeMap::new(),
                pinned_runtime_deps: BTreeSet::new(),
                extra_source_deps: BTreeMap::new(),
                extra_runtime_reexport_source_deps: BTreeMap::new(),
                extra_noop_deps: BTreeSet::new(),
            },
        }
    };
    let mut proposals = vec![
        // This looks best to the old greedy ordering because it saves more
        // lines, but it blocks both smaller cross-owner clusters.
        proposal("a", ModuleId(1), &["left", "right"], 100),
        proposal("b", ModuleId(2), &["left", "extra_left"], 1),
        proposal("c", ModuleId(3), &["right", "extra_right"], 1),
    ];
    super::sort_reader_migration_proposals_by_preference(&mut proposals);

    let selected = super::select_non_conflicting_reader_migration_proposals(&proposals)
        .into_iter()
        .map(|proposal| proposal.seed_binding)
        .collect::<BTreeSet<_>>();

    assert_eq!(
        selected,
        BTreeSet::from([BindingName::new("b"), BindingName::new("c")])
    );
}

#[test]
fn reader_migration_conflict_selector_rejects_cross_owner_reader_overlap() {
    let mut proposals = vec![
        RuntimeReaderClusterMigrationProposal {
            seed_binding: BindingName::new("a"),
            owner_module: ModuleId(1),
            source_lines: 1,
            migration: RuntimeReaderClusterMigration {
                primary_bindings: BTreeSet::from([BindingName::new("a")]),
                extra_snippets: BTreeSet::from([BindingName::new("sharedReader")]),
                extra_namespace_exports: BTreeSet::new(),
                extra_runtime_deps: BTreeSet::new(),
                extra_runtime_setter_deps: BTreeSet::new(),
                extra_runtime_dep_aliases: BTreeMap::new(),
                pinned_runtime_deps: BTreeSet::new(),
                extra_source_deps: BTreeMap::new(),
                extra_runtime_reexport_source_deps: BTreeMap::new(),
                extra_noop_deps: BTreeSet::new(),
            },
        },
        RuntimeReaderClusterMigrationProposal {
            seed_binding: BindingName::new("b"),
            owner_module: ModuleId(2),
            source_lines: 1,
            migration: RuntimeReaderClusterMigration {
                primary_bindings: BTreeSet::from([BindingName::new("b")]),
                extra_snippets: BTreeSet::from([BindingName::new("sharedReader")]),
                extra_namespace_exports: BTreeSet::new(),
                extra_runtime_deps: BTreeSet::new(),
                extra_runtime_setter_deps: BTreeSet::new(),
                extra_runtime_dep_aliases: BTreeMap::new(),
                pinned_runtime_deps: BTreeSet::new(),
                extra_source_deps: BTreeMap::new(),
                extra_runtime_reexport_source_deps: BTreeMap::new(),
                extra_noop_deps: BTreeSet::new(),
            },
        },
    ];
    super::sort_reader_migration_proposals_by_preference(&mut proposals);

    let selected = super::select_non_conflicting_reader_migration_proposals(&proposals);

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].seed_binding, BindingName::new("a"));
}

#[test]
fn namespace_getter_runtime_var_migration_moves_same_writer_export_targets() {
    let prelude = concat!(
        "var left;\n",
        "var right;\n",
        "var ns = {};\n",
        "function expose(target, exports) {}\n",
        "expose(ns, { left: () => left, right: () => right });\n",
    );
    let writer_body = "left = 1;\nright = 2;\nexport { left, right };\n";
    let consumer_body = "var value = ns.left;\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(!writer_source.contains("source-1-helpers"));
    assert!(writer_source.contains("var left, right;") || writer_source.contains("var left;"));
    assert!(writer_source.contains("var ns = {};"));
    assert!(writer_source.contains(
        "Object.defineProperties(ns, { left: { enumerable: true, get: () => left }, right: { enumerable: true, get: () => right } });"
    ));
    assert!(writer_source.contains("left = 1;"));
    assert!(writer_source.contains("right = 2;"));
    assert!(writer_source.contains("export { ns };"));
    assert!(
        consumer_source.contains("import { left } from './writer.js';"),
        "{consumer_source}"
    );
    assert!(consumer_source.contains("var value = left;"));
    assert!(!consumer_source.contains("ns.left"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn namespace_getter_runtime_var_migration_moves_cross_writer_export_targets() {
    let prelude = concat!(
        "var left;\n",
        "var right;\n",
        "var ns = {};\n",
        "function expose(target, exports) {}\n",
        "expose(ns, { left: () => left, right: () => right });\n",
    );
    let left_body = "left = 1;\nexport { left };\n";
    let right_body = "right = 2;\nexport { right };\n";
    let consumer_body = "var value = ns.left;\nexport { value };\n";
    let source = format!("{prelude}{left_body}{right_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "left-writer", "modules/left-writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + left_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "right-writer", "modules/right-writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + left_body.len()) as u32,
                (prelude.len() + left_body.len() + right_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + left_body.len() + right_body.len()) as u32,
                source.len() as u32,
            )),
    );

    let plan = plan_from_rows(rows);
    let left_source = planned_source(&plan, "modules/left-writer.ts");
    let right_source = planned_source(&plan, "modules/right-writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(!left_source.contains("__reverts_set_left"), "{left_source}");
    assert!(
        !right_source.contains("__reverts_set_right"),
        "{right_source}"
    );
    assert!(left_source.contains("import { right } from './right-writer.js';"));
    assert!(left_source.contains("var left;"));
    assert!(left_source.contains("var ns = {};"));
    assert!(left_source.contains(
        "Object.defineProperties(ns, { left: { enumerable: true, get: () => left }, right: { enumerable: true, get: () => right } });"
    ));
    assert!(right_source.contains("var right;"));
    assert!(
        consumer_source.contains("import { left } from './left-writer.js';"),
        "{consumer_source}"
    );
    assert!(consumer_source.contains("var value = left;"));
    assert!(!consumer_source.contains("ns.left"));
    assert!(
        planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
        "cross-writer namespace targets should no longer force a runtime helper"
    );
}

#[test]
fn runtime_namespace_member_access_rewrites_to_target_import_and_drops_namespace() {
    let prelude = concat!(
        "var shared = 1;\n",
        "var ns = {};\n",
        "function expose(target, exports) {}\n",
        "expose(ns, { value: () => shared });\n",
    );
    let consumer_body = "var value = ns.value + 1;\nexport { value };\n";
    let source = format!("{prelude}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        consumer_source.contains("import { shared } from './runtime/source-1-helpers.js';"),
        "{consumer_source}"
    );
    assert!(consumer_source.contains("var value = shared + 1;"));
    assert!(!consumer_source.contains("ns.value"));
    assert!(!consumer_source.contains("import { ns"));
    assert!(helper_source.contains("var shared = 1;"), "{helper_source}");
    assert!(!helper_source.contains("var ns = {};"), "{helper_source}");
    assert!(
        !helper_source.contains("Object.defineProperties(ns"),
        "{helper_source}"
    );
    assert!(
        !helper_source.contains("function expose"),
        "{helper_source}"
    );
}

#[test]
fn runtime_namespace_member_access_keeps_namespace_for_value_use() {
    let prelude = concat!(
        "var shared = 1;\n",
        "var ns = {};\n",
        "function expose(target, exports) {}\n",
        "expose(ns, { value: () => shared });\n",
    );
    let consumer_body = "var value = ns;\nexport { value };\n";
    let source = format!("{prelude}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        consumer_source.contains("import { ns } from './runtime/source-1-helpers.js';"),
        "{consumer_source}"
    );
    assert!(consumer_source.contains("var value = ns;"));
    assert!(helper_source.contains("var ns = {};"), "{helper_source}");
    assert!(
        helper_source.contains("Object.defineProperties(ns"),
        "{helper_source}"
    );
}

#[test]
fn runtime_namespace_member_access_rejects_local_name_collision() {
    let prelude = concat!(
        "var shared = 1;\n",
        "var ns = {};\n",
        "function expose(target, exports) {}\n",
        "expose(ns, { value: () => shared });\n",
    );
    let consumer_body =
        "var shared = 'local';\nvar value = ns.value + shared;\nexport { value };\n";
    let source = format!("{prelude}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(
        consumer_source.contains("var value = ns.value + shared;"),
        "{consumer_source}"
    );
    assert!(
        !consumer_source.contains("var value = __reverts_runtime_shared + shared;"),
        "{consumer_source}"
    );
    assert!(
        consumer_source.contains("import { ns } from './runtime/source-1-helpers.js';")
            || consumer_source.contains("var ns = {};"),
        "{consumer_source}"
    );
}

#[test]
fn runtime_namespace_member_access_rejects_writes_and_updates() {
    let prelude = concat!(
        "var shared = 1;\n",
        "var ns = {};\n",
        "function expose(target, exports) {}\n",
        "expose(ns, { value: () => shared });\n",
    );
    let consumer_body = "ns.value = 2;\nvar value = ns.value++;\nexport { value };\n";
    let source = format!("{prelude}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );

    let plan = plan_from_rows(rows);
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        consumer_source.contains("import { ns } from './runtime/source-1-helpers.js';"),
        "{consumer_source}"
    );
    assert!(consumer_source.contains("ns.value = 2;"));
    assert!(consumer_source.contains("var value = ns.value++;"));
    assert!(helper_source.contains("var ns = {};"), "{helper_source}");
    assert!(
        helper_source.contains("Object.defineProperties(ns"),
        "{helper_source}"
    );
}

#[test]
fn runtime_namespace_member_access_read_gate_rejects_compound_assignments() {
    for (source, start, end) in [
        ("ns.value += 1;", 0, 8),
        ("ns.value &&= 1;", 0, 8),
        ("ns.value ||= 1;", 0, 8),
        ("ns.value ??= 1;", 0, 8),
        ("ns.value **= 2;", 0, 8),
        ("ns.value <<= 1;", 0, 8),
        ("ns.value >>>= 1;", 0, 8),
        ("++ns.value;", 2, 10),
        ("ns.value--;", 0, 8),
    ] {
        assert!(
            !super::runtime_namespace_rewrite::runtime_namespace_member_access_site_is_read_only(
                source, start, end,
            ),
            "{source}"
        );
    }
    for (source, start, end) in [
        ("ns.value + 1;", 0, 8),
        ("ns.value && other;", 0, 8),
        ("ns.value || other;", 0, 8),
        ("ns.value ?? other;", 0, 8),
        ("ns.value >= 1;", 0, 8),
        ("ns.value === 1;", 0, 8),
    ] {
        assert!(
            super::runtime_namespace_rewrite::runtime_namespace_member_access_site_is_read_only(
                source, start, end,
            ),
            "{source}"
        );
    }
}

#[test]
fn single_reader_runtime_var_migration_reexports_cyclic_source_dependency_through_runtime() {
    let prelude = "var shared;\nfunction formatShared() { return decorate(shared); }\n";
    let writer_body = "shared = 'ok';\nexport { shared };\n";
    let helper_body = "function decorate(value) { return value; }\nexport { decorate };\n";
    let consumer_body = "var value = formatShared();\nexport { value };\n";
    let source = format!("{prelude}{writer_body}{helper_body}{consumer_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + writer_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "helper", "modules/helper.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len()) as u32,
                (prelude.len() + writer_body.len() + helper_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + writer_body.len() + helper_body.len()) as u32,
                source.len() as u32,
            )),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });

    let plan = plan_from_rows(rows);
    let writer_source = planned_source(&plan, "modules/writer.ts");
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        writer_source.contains("import { decorate } from './helper.js';"),
        "{writer_source}"
    );
    assert!(writer_source.contains("var shared;"));
    assert!(writer_source.contains("function formatShared() { return decorate(shared); }"));
    assert!(writer_source.contains("shared = 'ok';"));
    assert!(!writer_source.contains("__reverts_set_shared"));
    assert!(consumer_source.contains("import { formatShared } from './writer.js';"));
    assert!(helper_source.contains("import { decorate } from '../helper.js';"));
    assert!(!helper_source.contains("export { decorate };"));
    assert!(!helper_source.contains("var shared;"));
    assert!(!helper_source.contains("function formatShared()"));
    assert!(!helper_source.contains("__reverts_set_shared"));
}

#[test]
fn private_runtime_lazy_initializer_purifies_private_assignments() {
    let source = concat!(
        "var target;\n",
        "function value() { return 1; }\n",
        "var init = lazyValue(() => {\n",
        "\ttarget = value;\n",
        "});\n",
        "function read() { init(); return target; }\n",
    );
    let writable = BTreeSet::from([
        BindingName::new("target"),
        BindingName::new("init"),
        BindingName::new("value"),
        BindingName::new("read"),
    ]);
    let lowered = purify_private_runtime_lazy_initializers(source, &writable);

    assert!(!lowered.contains("= lazyValue("));
    assert!(lowered.contains("target = value;"));
    assert!(lowered.contains("var init = () => {};"));
    assert!(lowered.contains("function read() { init(); return target; }"));
}

#[test]
fn private_runtime_lazy_initializer_keeps_impure_bodies_lazy() {
    let source = concat!(
        "var target;\n",
        "function value() { return 1; }\n",
        "var init = lazyValue(() => {\n",
        "\tsetup();\n",
        "\ttarget = value;\n",
        "});\n",
    );
    let writable = BTreeSet::from([
        BindingName::new("target"),
        BindingName::new("init"),
        BindingName::new("value"),
    ]);
    let lowered = purify_private_runtime_lazy_initializers(source, &writable);

    assert!(lowered.contains("var init = lazyValue(() => {"));
}

#[test]
fn runtime_prelude_binding_written_by_module_uses_live_setter() {
    let planner = ImportExportPlanner;
    // Impure initializer keeps Phase 10c's migration plan
    // from picking this binding up — the test still validates that
    // the cross-module setter mechanism is wired correctly.
    let prelude = "var shared = makeShared();\n";
    let body = "shared = 1;\nexport { shared };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let entry_source = entry_file.body.join("\n");

    assert!(
        entry_source.contains(
            "import { shared, __reverts_set_shared } from './runtime/source-1-helpers.js';"
        )
    );
    assert!(entry_source.contains("__reverts_set_shared(1);"));
    assert!(entry_source.contains("export { shared };"));
    let helper_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
        .expect("runtime helper file should be planned");
    let helper_source = helper_file.body.join("\n");
    assert!(
        helper_source.contains("function __reverts_set_shared(value) { return shared = value; }")
    );
    let export_line = helper_source
        .lines()
        .find(|line| line.starts_with("export {"))
        .expect("helper should export runtime bindings");
    let exports = parse_generated_named_export_statement(export_line)
        .expect("helper should emit a generated export list");
    assert!(exports.contains(&BindingName::new("__reverts_set_shared")));
    assert!(exports.contains(&BindingName::new("shared")));
}

#[test]
fn write_only_runtime_prelude_binding_imports_setter_without_value() {
    let planner = ImportExportPlanner;
    // Impure initializer keeps the binding in runtime so this test
    // focuses on the setter import surface, not migration.
    let prelude = "var shared = makeShared();\n";
    let body = "shared = 1;\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_source = planned_source(&plan, "modules/entry.ts");

    assert!(
        entry_source
            .contains("import { __reverts_set_shared } from './runtime/source-1-helpers.js';")
    );
    assert!(!entry_source.contains("import { shared, __reverts_set_shared }"));
    assert!(entry_source.contains("__reverts_set_shared(1);"));
    assert!(!entry_source.contains("export { shared };"));
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");
    assert!(
        helper_source.contains("function __reverts_set_shared(value) { return shared = value; }")
    );
    let export_line = helper_source
        .lines()
        .find(|line| line.starts_with("export {"))
        .expect("helper should export runtime bindings");
    let exports = parse_generated_named_export_statement(export_line)
        .expect("helper should emit a generated export list");
    assert!(exports.contains(&BindingName::new("__reverts_set_shared")));
    assert!(!exports.contains(&BindingName::new("shared")));
}

#[test]
fn lazy_initializer_module_written_runtime_bindings_are_folded_into_helper() {
    let planner = ImportExportPlanner;
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared = Date.now();\n",
        "var Custom;\n",
    );
    let body = concat!(
        "var initShared = lazy(() => { shared = { ['required']: !1, matches: (A) => A.ready }; Custom = class Custom extends Error { constructor(A) { super(A); } }; });\n",
        "export { initShared, shared, Custom };\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let helper_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
        .expect("runtime helper file should be planned");
    let entry_source = entry_file.body.join("\n");
    let helper_source = helper_file.body.join("\n");

    assert!(
        entry_source.contains(
            "export { Custom, initShared, shared } from './runtime/source-1-helpers.js';"
        )
    );
    assert!(!entry_source.contains("__reverts_set_shared"));
    assert!(
        helper_source.contains("var shared = Date.now();"),
        "non-migratable initializer should keep the fixture folded:\n{helper_source}"
    );
    assert!(helper_source.contains("var Custom;"), "{helper_source}");
    assert!(helper_source.contains("shared = { ['required']: !1, matches: (A) => A.ready };"));
    assert!(
        helper_source
            .contains("Custom = class Custom extends Error { constructor(A) { super(A); } };")
    );
    assert!(helper_source.contains("var initShared = () => {};"));
    assert!(!helper_source.contains("_$init"));
    assert!(!helper_source.contains("_$val"));
    assert!(!helper_source.contains("var initShared = (() => {"));
    assert!(!helper_source.contains("__reverts_set_shared"));
    assert!(helper_source.contains("export { Custom, initShared, shared };"));
}

#[test]
fn impure_lazy_initializer_keeps_lazy_thunk_while_folding_into_helper() {
    let planner = ImportExportPlanner;
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared = Date.now();\n",
    );
    let body = concat!(
        "var initShared = lazy(() => { shared = Date.now(); });\n",
        "export { initShared, shared };\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let helper_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
        .expect("runtime helper file should be planned");
    let entry_source = entry_file.body.join("\n");
    let helper_source = helper_file.body.join("\n");

    assert!(
        entry_source
            .contains("export { initShared, shared } from './runtime/source-1-helpers.js';")
    );
    assert!(helper_source.contains("var initShared = lazyValue(() => {"));
    assert!(helper_source.contains("import { lazyValue } from './lazy.js';"));
    let lazy_source = planned_source(&plan, "modules/runtime/lazy.ts");
    assert!(lazy_source.contains("function lazyValue(factory) {"));
    assert!(helper_source.contains("shared = Date.now()"));
    assert!(!helper_source.contains("__reverts_set_shared"));
    assert!(helper_source.contains("export { initShared, shared };"));
}

#[test]
fn lazy_folded_stub_with_internal_consumers_is_bypassed() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared = Date.now();\n",
    );
    let folded_body = concat!(
        "var initShared = lazy(() => { shared = 1; });\n",
        "export { initShared, shared };\n",
    );
    let folded_source = format!("{prelude}{folded_body}");
    let consumer_source = "initShared();\nconsole.log(shared);\n";
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "folded.js",
        Some(folded_source.clone()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "consumer.js",
        Some(consumer_source.to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                folded_source.len() as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
            .with_source_file(2),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });

    let plan = plan_from_rows(rows);
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        planned_source_opt(&plan, "modules/folded.ts").is_none(),
        "pure re-export folded stub should be omitted"
    );
    assert!(
        consumer_source
            .contains("import { initShared, shared } from './runtime/source-1-helpers.js';")
    );
    assert!(!consumer_source.contains("from './folded.js'"));
    assert!(helper_source.contains("shared = 1;"));
    assert!(helper_source.contains("var initShared = () => {};"));
    assert!(helper_source.contains("export { initShared, shared };"));
}

#[test]
fn global_owner_rebuild_emits_noop_deps_in_folded_owner() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "function ownedA() { return Promise.resolve().catch(noop); }\n",
        "function noop() {}\n",
    );
    let folded_body = concat!(
        "var initUse = lazy(() => { ownedA(); });\n",
        "export { initUse, ownedA };\n",
    );
    let source = format!("{prelude}{folded_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    rows.symbols.push(SymbolInput::new(ModuleId(1), "ownedA"));

    let plan = plan_from_rows(rows);
    let folded_source = planned_source(&plan, "modules/folded.ts");
    let helper_source = planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts");

    assert!(
        folded_source.contains("function ownedA() { return Promise.resolve().catch(noop); }"),
        "{folded_source}"
    );
    assert!(
        folded_source.contains("function noop() {}"),
        "folded owner must get a local noop stub for moved snippets:\n{folded_source}"
    );
    if let Some(helper_source) = helper_source {
        assert!(
            !helper_source.contains("function ownedA()"),
            "{helper_source}"
        );
        assert!(
            !helper_source.contains("function noop() {}"),
            "{helper_source}"
        );
    }
}

#[test]
fn migrated_runtime_chunks_erase_call_only_noop_deps_in_folded_owner() {
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "function ownedA() { noop(); return 1; }\n",
        "function noop() {}\n",
    );
    let folded_body = concat!(
        "var initUse = lazy(() => { ownedA(); });\n",
        "export { initUse, ownedA };\n",
    );
    let source = format!("{prelude}{folded_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "folded", "modules/folded.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    rows.symbols.push(SymbolInput::new(ModuleId(1), "ownedA"));

    let plan = plan_from_rows(rows);
    let folded_source = planned_source(&plan, "modules/folded.ts");

    assert!(folded_source.contains("return 1;"), "{folded_source}");
    assert!(!folded_source.contains("noop();"), "{folded_source}");
    assert!(
        !folded_source.contains("function noop()"),
        "{folded_source}"
    );
}

#[test]
fn pure_reexport_stub_with_internal_consumers_is_bypassed() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "target.js",
        Some("export const value = 1;\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "barrel.js",
        Some("export { value } from './target.js';\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "consumer.js",
        Some("console.log(value);\n".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "target", "modules/target.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "barrel", "modules/barrel.ts").with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(planned_source_opt(&plan, "modules/barrel.ts").is_none());
    assert!(consumer_source.contains("import { value } from './target.js';"));
    assert!(!consumer_source.contains("from './barrel.js'"));
}

#[test]
fn pure_import_then_export_barrel_with_internal_consumers_is_bypassed() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "target.js",
        Some("export const value = 1;\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "barrel.js",
        Some("import { value } from './target.js';\nexport { value };\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "consumer.js",
        Some("console.log(value);\n".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "target", "modules/target.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "barrel", "modules/barrel.ts").with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let consumer_source = planned_source(&plan, "modules/consumer.ts");

    assert!(planned_source_opt(&plan, "modules/barrel.ts").is_none());
    assert!(consumer_source.contains("import { value } from './target.js';"));
    assert!(!consumer_source.contains("from './barrel.js'"));
}

#[test]
fn pure_reexport_alias_stub_is_not_bypassed() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "target.js",
        Some("export const value = 1;\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "barrel.js",
        Some("export { value as renamed } from './target.js';\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "consumer.js",
        Some("console.log(renamed);\n".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "target", "modules/target.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "barrel", "modules/barrel.ts").with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let barrel_source = planned_source(&plan, "modules/barrel.ts");

    assert!(consumer_source.contains("import { renamed } from './barrel.js';"));
    assert!(barrel_source.contains("export { value as renamed } from './target.js';"));
}

#[test]
fn import_alias_then_export_barrel_is_not_bypassed() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "target.js",
        Some("export const value = 1;\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "barrel.js",
        Some("import { value as renamed } from './target.js';\nexport { renamed };\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "consumer.js",
        Some("console.log(renamed);\n".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "target", "modules/target.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "barrel", "modules/barrel.ts").with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let barrel_source = planned_source(&plan, "modules/barrel.ts");

    assert!(consumer_source.contains("import { renamed } from './barrel.js';"));
    assert!(barrel_source.contains("import { value as renamed } from './target.js';"));
    assert!(barrel_source.contains("export { renamed };"));
}

#[test]
fn direct_reexport_with_extra_import_barrel_is_not_bypassed() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "target.js",
        Some("export const value = 1;\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "side.js",
        Some("export const side = 2;\nconsole.log(side);\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "barrel.js",
        Some(
            "import { side } from './side.js';\nexport { value } from './target.js';\n".to_string(),
        ),
    ));
    rows.source_files.push(SourceFileInput::new(
        4,
        "consumer.js",
        Some("console.log(value);\n".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "target", "modules/target.ts").with_source_file(1),
    );
    rows.modules
        .push(ModuleInput::application(ModuleId(2), "side", "modules/side.ts").with_source_file(2));
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "barrel", "modules/barrel.ts").with_source_file(3),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(4), "consumer", "modules/consumer.ts")
            .with_source_file(4),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(4),
        target: ModuleDependencyTarget::Module(ModuleId(3)),
    });

    let plan = plan_from_rows(rows);
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let barrel_source = planned_source(&plan, "modules/barrel.ts");

    assert!(consumer_source.contains("import { value } from './barrel.js';"));
    assert!(barrel_source.contains("import { side } from './side.js';"));
    assert!(barrel_source.contains("export { value } from './target.js';"));
}

#[test]
fn side_effectful_reexport_barrel_is_not_bypassed() {
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "target.js",
        Some("export const value = 1;\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        2,
        "barrel.js",
        Some("console.log('load barrel');\nexport { value } from './target.js';\n".to_string()),
    ));
    rows.source_files.push(SourceFileInput::new(
        3,
        "consumer.js",
        Some("console.log(value);\n".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "target", "modules/target.ts").with_source_file(1),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "barrel", "modules/barrel.ts").with_source_file(2),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
            .with_source_file(3),
    );
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(2),
        target: ModuleDependencyTarget::Module(ModuleId(1)),
    });
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(3),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });

    let plan = plan_from_rows(rows);
    let consumer_source = planned_source(&plan, "modules/consumer.ts");
    let barrel_source = planned_source(&plan, "modules/barrel.ts");

    assert!(consumer_source.contains("import { value } from './barrel.js';"));
    assert!(barrel_source.contains("console.log('load barrel')"));
    assert!(barrel_source.contains("export { value } from './target.js';"));
}

#[test]
fn lazy_initializer_fold_preserves_tail_side_effect_order_before_entrypoint() {
    let planner = ImportExportPlanner;
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared = Date.now();\n",
    );
    let body = concat!(
        "var initShared = lazy(() => { shared = 1; });\n",
        "export { initShared, shared };\n",
    );
    let tail = concat!(
        "var main = () => { console.log(shared); };\n",
        "initShared();\n",
        "main();\n",
    );
    let source = format!("{prelude}{body}{tail}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + body.len()) as u32,
            )),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let helper_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
        .expect("runtime helper file should be planned");
    let helper_source = helper_file.body.join("\n");
    let entrypoint_source = planned_source(&plan, "modules/entrypoint.ts");
    let init_assignment = helper_source
        .find("shared = 1;")
        .expect("pure initializer assignment should stay in runtime helper before imports finish");
    let shared_import = entrypoint_source
        .lines()
        .find(|line| line.contains("from './runtime/source-1-helpers.js';"))
        .filter(|line| line.contains("shared"))
        .and_then(|line| entrypoint_source.find(line))
        .expect("entrypoint island should import initialized shared binding");
    let tail_effect = entrypoint_source
        .find("console.log(shared)")
        .expect("entrypoint side effect should be preserved in the island");

    assert!(init_assignment < helper_source.len());
    assert!(shared_import < tail_effect);
    assert!(!helper_source.contains("\ninitShared();"));
    assert!(entrypoint_source.contains("initShared();"));
    assert!(helper_source.contains("var initShared = () => {};"));
    assert!(helper_source.contains("export { initShared"));
    assert!(helper_source.contains("shared };"));
}

#[test]
fn lazy_initializer_fold_imports_source_module_dependencies_from_helper() {
    let planner = ImportExportPlanner;
    let prelude = concat!(
        "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "var shared = Date.now();\n",
    );
    let body = concat!(
        "var initShared = lazy(() => { shared = buildValue(); });\n",
        "export { initShared, shared };\n",
    );
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.source_files.push(SourceFileInput::new(
        2,
        "dep.js",
        Some("function buildValue() { return 42; }\n".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    rows.modules
        .push(ModuleInput::application(ModuleId(2), "dep", "modules/dep.ts").with_source_file(2));
    rows.dependencies.push(ModuleDependencyInput {
        from_module_id: ModuleId(1),
        target: ModuleDependencyTarget::Module(ModuleId(2)),
    });
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let helper_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
        .expect("runtime helper file should be planned");
    let dep_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/dep.ts")
        .expect("source dependency should be planned");
    let helper_source = helper_file.body.join("\n");
    let dep_source = dep_file.body.join("\n");

    assert!(helper_source.contains("import { buildValue } from '../dep.js';"));
    assert!(helper_source.contains("shared = buildValue();"));
    assert!(dep_source.contains("export { buildValue };"));
}

#[test]
fn runtime_prelude_update_writes_use_live_setters() {
    let planner = ImportExportPlanner;
    // Impure initializer calls keep both bindings out of the Phase
    // 10c migration plan (pure number/object initializers would
    // otherwise be picked up). The numeric update-operator rewrite
    // path under test still runs against runtime-owned bindings.
    let prelude = concat!(
        "function makeCounter() { return 0; }\n",
        "function makeResult() { return 0; }\n",
        "var counter = makeCounter();\n",
        "var result = makeResult();\n",
    );
    let body = "result = counter--;\n++counter;\nexport { counter, result };\n";
    let source = format!("{prelude}{body}");
    assert!(super::implicit_global_writes_in_source(body).contains(&BindingName::new("counter")));
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let entry_source = entry_file.body.join("\n");

    assert!(entry_source.contains(
        "import { counter, result, __reverts_set_counter, __reverts_set_result } from './runtime/source-1-helpers.js';"
    ));
    assert!(entry_source.contains("__reverts_set_result((() => {"));
    assert!(entry_source.contains("let _$p = _$u--;"));
    assert!(entry_source.contains("let _$n = ++_$u;"));
    assert!(entry_source.contains("__reverts_set_counter(_$u);"));
    assert!(!entry_source.contains("counter--"));
    assert!(!entry_source.contains("++counter"));
}

#[test]
fn runtime_helper_files_import_source_module_dependencies_and_initialize_namespaces() {
    let planner = ImportExportPlanner;
    let prelude = concat!(
        "var ns = {};\n",
        "function expose(target, exports) {}\n",
        "expose(ns, { ready: () => ready });\n",
        "function ready() { return true; }\n",
        "function helper() { return Promise.resolve().then(() => (init(), ns)); }\n",
    );
    let entry_body = "var value = helper();\nexport { value };\n";
    let init_body = "var init = () => {};\n";
    let source = format!("{prelude}{entry_body}{init_body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                prelude.len() as u32,
                (prelude.len() + entry_body.len()) as u32,
            )),
    );
    rows.modules.push(
        ModuleInput::application(ModuleId(2), "init", "modules/init.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(
                (prelude.len() + entry_body.len()) as u32,
                source.len() as u32,
            )),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let helper_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
        .expect("runtime helper file should be planned");
    let helper_source = helper_file.body.join("\n");
    let init_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/init.ts")
        .expect("init file should be planned");
    let init_source = init_file.body.join("\n");

    assert!(helper_source.contains("import { init } from '../init.js';"));
    assert!(helper_source.contains("Object.defineProperties(ns,"));
    assert!(helper_source.contains("get: () => ready"));
    assert!(init_source.contains("export { init };"));
}

#[test]
fn runtime_prelude_array_destructuring_writes_use_live_setters() {
    let planner = ImportExportPlanner;
    // Impure initializer calls keep both bindings out of the Phase
    // 10c migration plan — the destructuring rewrite path under
    // test still routes writes through the cross-module setter.
    let prelude = concat!(
        "function makeLeft() { return {}; }\n",
        "function makeRight() { return {}; }\n",
        "var left = makeLeft();\n",
        "var right = makeRight();\n",
    );
    let body = "[left, right] = [1, 2];\nexport { left, right };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let entry_source = entry_file.body.join("\n");

    assert!(entry_source.contains(
        "import { left, right, __reverts_set_left, __reverts_set_right } from './runtime/source-1-helpers.js';"
    ));
    assert!(entry_source.contains("__reverts_set_left(_$t[0]);"));
    assert!(entry_source.contains("__reverts_set_right(_$t[1]);"));
    assert!(!entry_source.contains("[left, right] ="));
}

#[test]
fn runtime_prelude_write_inside_computed_class_key_uses_live_setter() {
    let planner = ImportExportPlanner;
    // Impure initializer calls keep the prelude vars out of the
    // Phase 10c migration plan — the test still exercises the
    // setter rewrite inside a computed class key.
    let prelude = concat!(
        "var J = (init, value) => () => (init && (value = init(init = 0)), value);\n",
        "function makeStream() { return {}; }\n",
        "function makeHolder() { return {}; }\n",
        "var Stream = makeStream();\n",
        "var holder = makeHolder();\n",
    );
    let body = "void 0;\nvar init = J(() => { Stream = class Stream { [(holder = new WeakMap(), Symbol.iterator)]() { return 1; } }; });\nexport { Stream, init };\n";
    let source = format!("{prelude}{body}");
    assert!(super::implicit_global_writes_in_source(body).contains(&BindingName::new("holder")));
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let entry_source = entry_file.body.join("\n");

    assert!(entry_source.contains("__reverts_set_Stream"));
    assert!(entry_source.contains("__reverts_set_holder"));
    assert!(entry_source.contains("__reverts_set_holder(new WeakMap())"));
    assert!(!entry_source.contains("holder = new WeakMap()"));
}

#[test]
fn runtime_helper_namespace_exports_initialize_empty_object_helpers() {
    let planner = ImportExportPlanner;
    // Impure initializer keeps d2 out of the Phase 10c migration
    // plan — this test continues to validate the namespace-export
    // setup alongside cross-module setter use.
    let prelude = concat!(
        "var zT = () => 'enum';\n",
        "var m = {};\n",
        "M5(m, { enum: () => zT });\n",
        "function makeD2() { return {}; }\n",
        "var d2 = makeD2();\n",
    );
    let body = "d2 = m;\nexport { d2 };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let helper_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
        .expect("runtime helper file should be planned");
    let entry_source = entry_file.body.join("\n");
    let helper_source = helper_file.body.join("\n");

    assert!(
        entry_source
            .contains("import { d2, m, __reverts_set_d2 } from './runtime/source-1-helpers.js';")
    );
    assert!(helper_source.contains("var zT = () => 'enum';"));
    assert!(helper_source.contains("var m = {};"));
    assert!(
        helper_source
            .contains("Object.defineProperties(m, { enum: { enumerable: true, get: () => zT } });")
    );
    assert!(helper_source.contains("export { __reverts_set_d2, d2, m };"));
}

#[test]
fn array_member_assignment_is_not_treated_as_binding_pattern() {
    assert!(super::array_destructuring_assignment_writes("[this.value] = values;", 0).is_none());
    assert!(super::array_destructuring_assignment_writes("object[key] = value;", 6).is_none());
}

#[test]
fn template_interpolation_singleton_literal_is_inlined_from_prelude() {
    let planner = ImportExportPlanner;
    let prelude = "var EDL = 'date';\n";
    let body = "var value = new RegExp(`^${EDL}$`);\nexport { value };\n";
    let source = format!("{prelude}{body}");
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files
        .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
            .with_source_file(1)
            .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_file = plan
        .files
        .iter()
        .find(|file| file.path == "modules/entry.ts")
        .expect("entry file should be planned");
    let entry_source = entry_file.body.join("\n");

    assert!(entry_source.contains("var EDL = 'date';"));
    assert!(entry_source.contains("var value = new RegExp(`^${EDL}$`);"));
    assert!(!entry_source.contains("source-1-helpers"));
    assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
}

#[test]
fn bare_commonjs_require_gets_esm_create_require_bridge() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some("var crypto = require('crypto');\nexport { crypto };".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_source = plan.files[0].body.join("\n");

    assert!(entry_source.contains("import { createRequire } from 'node:module';"));
    assert!(entry_source.contains("var require = createRequire(import.meta.url);"));
    assert!(entry_source.contains("require('crypto')"));
}

#[test]
fn require_bridge_does_not_redeclare_implicit_require_global() {
    let planner = ImportExportPlanner;
    let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
    rows.source_files.push(SourceFileInput::new(
        1,
        "bundle.js",
        Some("require('fs');\nrequire = require;\nexport const value = 1;".to_string()),
    ));
    rows.modules.push(
        ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
    );
    let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
    let model = ProgramModel::from_input(input);
    let enriched = reverts_model::EnrichedProgram::new(
        model,
        reverts_model::SemanticNameMap::default(),
        Vec::new(),
        reverts_ir::BindingShapeSolution::default(),
    );

    let plan = planner
        .plan_enriched_program(&enriched)
        .expect("fixture should normalize");
    let entry_source = plan.files[0].body.join("\n");

    assert!(entry_source.contains("var require = createRequire(import.meta.url);"));
    assert!(!entry_source.contains("const require ="));
    assert!(!entry_source.contains("var require;\n"));
}
