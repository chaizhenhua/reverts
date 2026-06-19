use std::path::Path;

use super::{
    CompilerLowering, FormatSourceRequest, GeneratedExport, GeneratedImport, GeneratedRename,
    ImportUsageScope, JsError, LazyBodyClassification, ParseGoal, TopLevelStatementKind,
    classify_import_usage_scope, classify_lazy_module_body,
    collect_file_url_source_location_rewrites, collect_identifier_read_facts,
    collect_path_builder_calls, collect_static_resource_specifiers,
    collect_static_template_literals, collect_string_literals, collect_top_level_statement_facts,
    collect_type_coverage_stats, collect_void_zero_expression_statements,
    extract_lazy_module_eager_value, format_source_minified, format_source_pretty,
    format_source_with_module_items, format_source_with_module_items_and_renames,
    format_source_with_module_items_and_renames_with_report,
    format_source_with_module_items_request, lazy_value_sub_snippets,
    normalize_source_for_pipeline, parse_error_message, parse_options_for, parse_source,
    sanitize_identifier, skip_block_comment, skip_line_comment,
    verify_only_immediate_call_references,
};
use std::collections::BTreeSet;

#[test]
fn shared_parser_options_allow_top_level_return() {
    let source_type = super::source_type_for_parse(None, ParseGoal::JavaScript);
    assert!(parse_options_for(source_type).allow_return_outside_function);
}

#[test]
fn shared_identifier_helpers_cover_contextual_keywords_and_comments() {
    assert!(super::is_ascii_identifier_start(b'$'));
    assert!(super::is_ascii_identifier_continue(b'9'));
    assert!(super::is_js_keyword("async"));
    assert_eq!(skip_line_comment(b"// comment\nnext", 2), 10);
    assert_eq!(skip_block_comment(b"/* block */next", 2), 11);
}

#[test]
fn parses_typescript_without_external_tooling() {
    let source = "const answer: number = 42;";

    assert!(parse_source(source, Some(Path::new("fixture.ts")), ParseGoal::TypeScript).is_ok());
}

#[test]
fn collects_string_literal_facts_from_ast_only() {
    let source = "import './tree-sitter.wasm';\nconst native = require('/$bunfs/root/addon.node');";

    let literals =
        collect_string_literals(source, Some(Path::new("fixture.ts")), ParseGoal::TypeScript)
            .expect("string literals should be collected from parseable source");

    let values = literals
        .iter()
        .map(|literal| literal.value.as_str())
        .collect::<Vec<_>>();
    assert!(values.contains(&"./tree-sitter.wasm"));
    assert!(values.contains(&"/$bunfs/root/addon.node"));
    assert!(
        literals
            .iter()
            .all(|literal| literal.byte_end > literal.byte_start)
    );
}

#[test]
fn collects_static_template_literals_without_touching_tagged_or_interpolated() {
    let source = r#"
        const docs = `one
two`;
        const nested = `${`inner
value`}-${name}`;
        const tagged = tag`raw
value`;
    "#;

    let literals = collect_static_template_literals(
        source,
        Some(Path::new("fixture.ts")),
        ParseGoal::TypeScript,
    )
    .expect("template literals should be collected from parseable source");

    let values = literals
        .iter()
        .map(|literal| literal.value.as_str())
        .collect::<Vec<_>>();
    assert!(values.contains(&"one\ntwo"));
    assert!(values.contains(&"inner\nvalue"));
    assert!(!values.contains(&"raw\nvalue"));
    assert!(
        literals
            .iter()
            .all(|literal| literal.byte_end > literal.byte_start)
    );
}

#[test]
fn collects_top_level_statement_facts_for_runtime_attribution() {
    let source = "import { dep } from './dep.js';\n\
                  var data = lazyValue(() => ({ dep }));\n\
                  var regular = 1;\n\
                  function __reverts_set_data(value) { data = value; return value; }\n\
                  async function run() { return data(); }\n\
                  class Box {}\n\
                  export { data, run };\n";

    let facts = collect_top_level_statement_facts(
        source,
        Some(Path::new("runtime.ts")),
        ParseGoal::TypeScript,
    )
    .expect("runtime statements should parse");

    let kinds = facts.iter().map(|fact| fact.kind).collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            TopLevelStatementKind::Import,
            TopLevelStatementKind::LazyValue,
            TopLevelStatementKind::Variable,
            TopLevelStatementKind::Setter,
            TopLevelStatementKind::Function,
            TopLevelStatementKind::Class,
            TopLevelStatementKind::Export,
        ]
    );
    assert_eq!(facts[1].bindings, vec!["data"]);
    assert_eq!(facts[3].bindings, vec!["__reverts_set_data"]);
    assert_eq!(facts[4].bindings, vec!["run"]);
    assert!(
        facts.iter().all(|fact| fact.byte_end > fact.byte_start),
        "statement spans must be non-empty: {facts:?}"
    );
}

#[test]
fn collects_void_zero_expression_statement_spans() {
    let source = "function f() { void 0; return void 0; }\nif (ok) void 0;\n{ void 0; }\nvoid 0;\n";

    let facts = collect_void_zero_expression_statements(
        source,
        Some(Path::new("runtime.ts")),
        ParseGoal::TypeScript,
    )
    .expect("source should parse");
    let slices = facts
        .iter()
        .map(|fact| &source[fact.byte_start as usize..fact.byte_end as usize])
        .collect::<Vec<_>>();

    assert_eq!(source.matches("void 0;").count(), 5);
    assert_eq!(slices, vec!["void 0;", "void 0;", "void 0;"]);
}

#[test]
fn collects_identifier_reads_from_ast_without_string_scanner() {
    let source = r#"
        const { local } = source;
        const copy = [...shared, local];
        class Transport extends Base {
            field = Buffer.alloc(0);
            method(event) { return event.ready + shared; }
        }
        packageInit();
        object.packageInit();
        new Constructed();
        const template = `${source}-${shared}`;
        const ignored = "packageInit()";
        export { shared as exportedShared };
        export { externalOnly } from "./external";
    "#;

    let facts =
        collect_identifier_read_facts(source, Some(Path::new("fixture.ts")), ParseGoal::TypeScript)
            .expect("identifier reads should be collected from parseable source");
    let names = facts
        .iter()
        .map(|fact| fact.name.as_str())
        .collect::<BTreeSet<_>>();
    assert!(names.contains("source"));
    assert!(names.contains("shared"));
    assert!(names.contains("Base"));
    assert!(names.contains("Buffer"));
    assert!(names.contains("event"));
    assert!(names.contains("packageInit"));
    assert!(names.contains("object"));
    assert!(names.contains("Constructed"));
    assert!(names.contains("shared"));
    assert!(!names.contains("Transport"));
    assert!(!names.contains("field"));
    assert!(!names.contains("method"));
    assert!(!names.contains("ready"));
    assert!(!names.contains("ignored"));
    assert!(!names.contains("externalOnly"));

    let callees = facts
        .iter()
        .filter(|fact| fact.is_call_callee)
        .map(|fact| fact.name.as_str())
        .collect::<BTreeSet<_>>();
    assert!(callees.contains("packageInit"));
    assert!(callees.contains("Constructed"));
    assert!(!callees.contains("alloc"));
    assert_eq!(
        facts
            .iter()
            .find(|fact| fact.name == "packageInit" && fact.is_call_callee)
            .and_then(|fact| fact.call_arg_count),
        Some(0)
    );
    assert_eq!(
        facts
            .iter()
            .find(|fact| fact.name == "Constructed" && fact.is_call_callee)
            .and_then(|fact| fact.call_arg_count),
        None
    );
}

#[test]
fn collects_static_resource_specifiers_from_ast_contexts_only() {
    let source = r#"
        import './style.css';
        export * from './icons.svg';
        const native = require('/$bunfs/root/addon.node');
        const wasm = new URL('./parser.wasm', import.meta.url);
        const ignored = 'bash.exe';
    "#;

    let specifiers = collect_static_resource_specifiers(
        source,
        Some(Path::new("fixture.ts")),
        ParseGoal::TypeScript,
    )
    .expect("static resource specifiers should be collected");

    let values = specifiers
        .iter()
        .map(|literal| literal.value.as_str())
        .collect::<Vec<_>>();
    assert!(values.contains(&"./style.css"));
    assert!(values.contains(&"./icons.svg"));
    assert!(values.contains(&"/$bunfs/root/addon.node"));
    assert!(values.contains(&"./parser.wasm"));
    assert!(!values.contains(&"bash.exe"));
}

#[test]
fn collects_file_url_source_location_rewrite_spans_from_ast_context() {
    let source = "const here = url.fileURLToPath('file:///home/runner/work/app/src/tool.ts');";

    let rewrites = collect_file_url_source_location_rewrites(
        source,
        Some(Path::new("fixture.ts")),
        ParseGoal::TypeScript,
    )
    .expect("file url source location should parse");

    assert_eq!(rewrites.len(), 1);
    assert_eq!(
        rewrites[0].value,
        "file:///home/runner/work/app/src/tool.ts"
    );
    assert_eq!(
        &source[rewrites[0].byte_start as usize..rewrites[0].byte_end as usize],
        "'file:///home/runner/work/app/src/tool.ts'"
    );
}

#[test]
fn collects_path_builder_string_arguments_from_ast_context() {
    let source = "\
        const vendor = path.resolve(root, 'vendor', 'ripgrep');\n\
        const command = path.resolve(vendor, 'x64-linux', 'rg');\n\
        const inert = ['vendor', 'ripgrep', 'rg'];";

    let calls =
        collect_path_builder_calls(source, Some(Path::new("fixture.ts")), ParseGoal::TypeScript)
            .expect("path builder calls should parse");

    let arguments = calls
        .iter()
        .map(|call| call.string_arguments.as_slice())
        .collect::<Vec<_>>();
    assert!(arguments.contains(&["vendor".to_string(), "ripgrep".to_string()].as_slice()));
    assert!(arguments.contains(&["x64-linux".to_string(), "rg".to_string()].as_slice()));
    assert_eq!(calls.len(), 2);
}

#[test]
fn reports_parse_failure_without_panicking() {
    let error = parse_source("const =", None, ParseGoal::TypeScript);

    assert!(matches!(error, Err(JsError::ParseFailed(errors)) if !errors.is_empty()));
}

#[test]
fn shared_parse_error_message_uses_first_diagnostic() {
    let error =
        parse_source("const =", None, ParseGoal::TypeScript).expect_err("fixture should not parse");

    let message = parse_error_message(&error, "source could not be parsed");

    assert!(message.starts_with("source could not be parsed as"));
}

#[test]
fn formats_typescript_through_oxc_codegen() {
    let formatted = format_source_pretty("const x:number=1", None, ParseGoal::TypeScript)
        .expect("fixture should parse");

    assert!(formatted.contains("const x: number = 1"));
}

#[test]
fn minifies_typescript_through_oxc_codegen() {
    let formatted = format_source_minified(
        "const x = { alpha: ['a', 'b c'] };",
        None,
        ParseGoal::TypeScript,
    )
    .expect("fixture should parse");

    assert_eq!(formatted, "const x={alpha:['a','b c']};");
}

#[test]
fn pipeline_normalization_uses_ast_codegen() {
    let normalized = normalize_source_for_pipeline("export function add(a,b){return a+b}", None)
        .expect("fixture should normalize");

    assert!(normalized.contains("export function add(a, b)"));
    assert!(normalized.contains("return a + b;"));
}

#[test]
fn module_item_formatting_infers_safe_literal_variable_types_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "var label = 'ready'; function f() { let count = 1; const ok = true; }",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("var label: string = 'ready';"));
    assert!(formatted.contains("let count: number = 1;"));
    assert!(formatted.contains("const ok: boolean = true;"));
}

#[test]
fn module_item_formatting_skips_reassigned_literal_variable_types_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "let status = 'ready'; status = next(); const stable = 'ok';",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("let status = 'ready';"));
    assert!(formatted.contains("const stable: string = 'ok';"));
}

#[test]
fn module_item_formatting_infers_minified_unary_literal_types_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "var missing = -1; const present = +1; let enabled = !0;",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("var missing: number = -1;"));
    assert!(formatted.contains("const present: number = +1;"));
    assert!(formatted.contains("let enabled: boolean = !0;"));
}

#[test]
fn module_item_formatting_infers_fixed_operator_result_types_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "const kind = typeof value; const removed = delete target.key; const same = left === right; const total = 1 + 2 * 3;",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("const kind: string = typeof value;"));
    assert!(formatted.contains("const removed: boolean = delete target.key;"));
    assert!(formatted.contains("const same: boolean = left === right;"));
    assert!(formatted.contains("const total: number = 1 + 2 * 3;"));
}

#[test]
fn module_item_formatting_infers_builtin_api_result_types_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "const stamp = Date.now(); const text = JSON.stringify(value); const finite = Number.isFinite(value); const keys = Object.keys(value); const env = process.env.HOME; const choice = flag ? 'yes' : 'no';",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("const stamp: number = Date.now();"));
    assert!(formatted.contains("const text: string = JSON.stringify(value);"));
    assert!(formatted.contains("const finite: boolean = Number.isFinite(value);"));
    assert!(formatted.contains("const keys: string[] = Object.keys(value);"));
    assert!(formatted.contains("const env: string | undefined = process.env.HOME;"));
    assert!(formatted.contains("const choice: string = flag ? 'yes' : 'no';"));
}

#[test]
fn module_item_formatting_infers_default_parameter_and_return_types_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "function answer(input = 1) { return 42; } const echo = (label = 'ok') => { return 'ready'; };",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("function answer(input: number = 1): number"));
    assert!(formatted.contains("const echo = (label: string = 'ok'): string =>"));
}

#[test]
fn module_item_formatting_infers_parameters_from_call_sites_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "function greet(name) { return 'hi'; } greet('Ada'); const double = (value) => value; double(2);",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("function greet(name: string): string"));
    assert!(formatted.contains("const double = (value: number) =>"));
}

#[test]
fn type_coverage_stats_count_annotatable_boundaries() {
    let stats = collect_type_coverage_stats(
        "const typed: string = 'x'; let plain = 1; function f(a: number, b) { return a; } const g = (x: boolean): boolean => x;",
        Some(Path::new("fixture.ts")),
        ParseGoal::TypeScript,
    )
    .expect("fixture should parse");

    assert_eq!(stats.variable_candidates, 3);
    assert_eq!(stats.variable_annotated, 1);
    assert_eq!(stats.parameter_candidates, 3);
    assert_eq!(stats.parameter_annotated, 2);
    assert_eq!(stats.return_candidates, 2);
    assert_eq!(stats.return_annotated, 1);
    assert_eq!(stats.total_candidates(), 8);
    assert_eq!(stats.total_annotated(), 4);
}

#[test]
fn module_item_formatting_infers_union_call_site_parameters_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "function mixed(value) { return true; } mixed(1); mixed('no');",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("function mixed(value: number | string): boolean"));
}

#[test]
fn module_item_formatting_infers_union_return_types_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "function mixed(flag) { if (flag) return 1; return 'no'; }",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("function mixed(flag)"));
    assert!(formatted.contains("function mixed(flag): number | string"));
}

#[test]
fn module_item_formatting_infers_structural_return_types_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "function config() { return { name: 'cli', ports: [80, 443] }; }",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("function config(): {"));
    assert!(formatted.contains("name: string;"));
    assert!(formatted.contains("ports: number[];"));
}

#[test]
fn module_item_formatting_infers_object_and_array_literal_types_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "const tuple = [1, 2]; const config = { name: 'cli', port: 443 };",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("const tuple: number[] = [1, 2];"));
    assert!(formatted.contains("const config: {"));
    assert!(formatted.contains("name: string;"));
    assert!(formatted.contains("port: number;"));
}

#[test]
fn module_item_formatting_infers_structural_array_literal_types_when_requested() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "const variants = [1, 'x']; const rows = [{ id: 1, name: 'a' }, { id: 2, name: 'b' }];",
        generated_imports: &[],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("const variants: (number | string)[] = ["));
    assert!(formatted.contains("const rows: {"));
    assert!(formatted.contains("id: number;"));
    assert!(formatted.contains("name: string;"));
    assert!(formatted.contains("}[] = ["));
}

#[test]
fn module_item_formatting_recovers_package_member_type_query() {
    let formatted = format_source_with_module_items_request(FormatSourceRequest {
        body_source: "const createClient = __pkg.createClient;",
        generated_imports: &[GeneratedImport::new("__pkg", "pkg")],
        generated_exports: &[],
        readability_renames: &[],
        type_annotations: &[],
        infer_literal_types: true,
        path_hint: Some(Path::new("fixture.ts")),
        goal: ParseGoal::TypeScript,
        lowering: CompilerLowering::None,
    })
    .expect("fixture should format");

    assert!(formatted.contains("import * as pkg from 'pkg';"));
    assert!(formatted.contains("const createClient: typeof pkg.createClient = pkg.createClient;"));
}

#[test]
fn module_item_formatting_builds_imports_and_exports_as_ast_nodes() {
    let formatted = format_source_with_module_items(
        "const answer = __pkg.answer;",
        &[GeneratedImport::new("__pkg", "pkg")],
        &[GeneratedExport::new("answer")],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import * as pkg from 'pkg';"));
    assert!(formatted.contains("const answer = pkg.answer;"));
    assert!(formatted.contains("export { answer };"));
}

#[test]
fn module_item_formatting_emits_import_attributes() {
    let formatted = format_source_with_module_items(
        "const aliceblue = colors.default.aliceblue;",
        &[GeneratedImport::new("colors", "css-color-names").with_attribute("type", "json")],
        &[],
        Some(Path::new("modules/runtime/source-1-colors.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import * as colors from 'css-color-names' with { type: 'json' };"));
    assert!(formatted.contains("const aliceblue = colors.default.aliceblue;"));
}

#[test]
fn module_item_formatting_coalesces_named_imports_by_source() {
    let formatted = format_source_with_module_items(
        "import { join as localJoin } from 'path';\nimport * as pathNS from 'path';\nimport { dirname as localDir, join as otherJoin } from 'path';\nconsole.log(pathNS, localJoin, localDir, otherJoin);",
        &[],
        &[],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { dirname, join, join as otherJoin } from 'path';"));
    assert!(formatted.contains("import * as pathNS from 'path';"));
    assert!(formatted.contains("console.log(pathNS, join, dirname, otherJoin);"));
    assert_eq!(formatted.matches("from 'path'").count(), 2);
}

#[test]
fn module_item_formatting_keeps_namespace_and_named_imports_separate() {
    let formatted = format_source_with_module_items(
        "import * as pkgNS from 'pkg';\nimport { alpha } from 'pkg';\nimport { beta } from 'pkg';\nconsole.log(pkgNS, alpha, beta);",
        &[],
        &[],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import * as pkgNS from 'pkg';"));
    assert!(formatted.contains("import { alpha, beta } from 'pkg';"));
    assert_eq!(formatted.matches("from 'pkg'").count(), 2);
}

#[test]
fn module_item_formatting_merges_default_and_named_imports() {
    let formatted = format_source_with_module_items(
        "import defaultPkg from 'pkg';\nimport { alpha } from 'pkg';\nimport { beta as localBeta } from 'pkg';\nconsole.log(defaultPkg, alpha, localBeta);",
        &[],
        &[],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import defaultPkg, { alpha, beta } from 'pkg';"));
    assert!(formatted.contains("console.log(defaultPkg, alpha, beta);"));
    assert_eq!(formatted.matches("from 'pkg'").count(), 1);
}

#[test]
fn module_item_formatting_prunes_unused_named_import_specifiers() {
    let formatted = format_source_with_module_items(
        "import { used, unused } from 'pkg';\nconsole.log(used);",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { used } from 'pkg';"));
    assert!(!formatted.contains("unused"));
}

#[test]
fn module_item_formatting_keeps_imports_used_by_local_exports() {
    let formatted = format_source_with_module_items(
        "import { used, exported, unused } from 'pkg';\nconsole.log(used);\nexport { exported };",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { exported, used } from 'pkg';"));
    assert!(formatted.contains("export { exported };"));
    assert!(!formatted.contains("unused"));
}

#[test]
fn module_item_formatting_keeps_all_unused_import_statement_for_side_effects() {
    let formatted = format_source_with_module_items(
        "import { unused } from 'pkg';\nconsole.log(1);",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { unused } from 'pkg';"));
}

#[test]
fn module_item_formatting_prunes_unused_default_from_mixed_import() {
    let formatted = format_source_with_module_items(
        "import defaultPkg, { used, unused } from 'pkg';\nconsole.log(used);",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { used } from 'pkg';"));
    assert!(!formatted.contains("defaultPkg"));
    assert!(!formatted.contains("unused"));
}

#[test]
fn module_item_formatting_merges_default_and_namespace_imports() {
    let formatted = format_source_with_module_items(
        "import defaultPkg from 'pkg';\nimport * as pkgNS from 'pkg';\nconsole.log(defaultPkg, pkgNS);",
        &[],
        &[],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import defaultPkg, * as pkgNS from 'pkg';"));
    assert_eq!(formatted.matches("from 'pkg'").count(), 1);
}

#[test]
fn module_item_formatting_keeps_default_named_import_when_namespace_exists() {
    let formatted = format_source_with_module_items(
        "import defaultPkg, { alpha } from 'pkg';\nimport * as pkgNS from 'pkg';\nconsole.log(defaultPkg, alpha, pkgNS);",
        &[],
        &[],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import defaultPkg, { alpha } from 'pkg';"));
    assert!(formatted.contains("import * as pkgNS from 'pkg';"));
    assert_eq!(formatted.matches("from 'pkg'").count(), 2);
}

#[test]
fn module_item_formatting_merges_multiple_default_aliases_as_named_default() {
    let formatted = format_source_with_module_items(
        "import first from 'pkg';\nimport second from 'pkg';\nimport { alpha } from 'pkg';\nconsole.log(first, second, alpha);",
        &[],
        &[],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import first, { alpha, default as second } from 'pkg';"));
    assert_eq!(formatted.matches("from 'pkg'").count(), 1);
}

#[test]
fn module_item_formatting_merges_duplicate_default_named_imports() {
    let formatted = format_source_with_module_items(
        "import first, { alpha } from 'pkg';\nimport second, { beta } from 'pkg';\nconsole.log(first, second, alpha, beta);",
        &[],
        &[],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import first, { alpha, beta, default as second } from 'pkg';"));
    assert_eq!(formatted.matches("from 'pkg'").count(), 1);
}

#[test]
fn module_item_formatting_keeps_duplicate_default_namespace_imports() {
    let formatted = format_source_with_module_items(
        "import firstDefault, * as firstNS from 'pkg';\nimport secondDefault, * as secondNS from 'pkg';\nconsole.log(firstDefault, firstNS, secondDefault, secondNS);",
        &[],
        &[],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import firstDefault, * as firstNS from 'pkg';"));
    assert!(formatted.contains("import secondDefault, * as secondNS from 'pkg';"));
    assert_eq!(formatted.matches("from 'pkg'").count(), 2);
}

#[test]
fn module_item_formatting_flattens_node_builtin_namespace_members() {
    let formatted = format_source_with_module_items(
        "import * as pathNS from 'path';\nconsole.log(pathNS.join('a', 'b'), pathNS.resolve('x'));",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { join, resolve } from 'path';"));
    assert!(formatted.contains("console.log(join('a', 'b'), resolve('x'));"));
    assert!(!formatted.contains("pathNS."));
}

#[test]
fn module_item_formatting_reuses_existing_named_alias_for_namespace_member() {
    let formatted = format_source_with_module_items(
        "import { join as j } from 'path';\nimport * as pathNS from 'path';\nconsole.log(pathNS.join('a', 'b'));",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { join } from 'path';"));
    assert!(formatted.contains("console.log(join('a', 'b'));"));
    assert_eq!(formatted.matches("from 'path'").count(), 1);
}

#[test]
fn module_item_formatting_flattens_default_namespace_builtin_import() {
    let formatted = format_source_with_module_items(
        "import pathDefault, * as pathNS from 'path';\nconsole.log(pathNS.join('a', 'b'), pathDefault.sep);",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(
        formatted.contains("import pathDefault, { join as __reverts_pathNS_join } from 'path';"),
        "{formatted}"
    );
    assert!(formatted.contains("console.log(__reverts_pathNS_join('a', 'b'), pathDefault.sep);"));
    assert!(!formatted.contains("pathNS."));
    assert_eq!(formatted.matches("from 'path'").count(), 1);
}

#[test]
fn module_item_formatting_keeps_namespace_import_used_as_value() {
    let formatted = format_source_with_module_items(
        "import * as pathNS from 'path';\nconsole.log(pathNS, pathNS.join('a', 'b'));",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import * as pathNS from 'path';"));
    assert!(formatted.contains("pathNS.join"));
}

#[test]
fn module_item_formatting_keeps_exported_namespace_imports() {
    let formatted = format_source_with_module_items(
        "import * as pathNS from 'path';\nconsole.log(pathNS.join('a', 'b'));\nexport { pathNS };",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import * as pathNS from 'path';"));
    assert!(formatted.contains("pathNS.join"));
    assert!(formatted.contains("export { pathNS };"));
}

#[test]
fn module_item_formatting_keeps_non_builtin_namespace_imports() {
    let formatted = format_source_with_module_items(
        "import * as pkgNS from 'pkg';\nconsole.log(pkgNS.join('a', 'b'));",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { join } from 'pkg';"));
    assert!(formatted.contains("console.log(join('a', 'b'));"));
}

#[test]
fn module_item_formatting_inlines_single_use_tiny_return_helper() {
    let formatted = format_source_with_module_items(
        "const value = 1;\nfunction readValue() { return value; }\nconsole.log(readValue());",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(!formatted.contains("function readValue"));
    assert!(formatted.contains("console.log(value);"));
}

#[test]
fn module_item_formatting_inlines_single_use_const_arrow_helper() {
    let formatted = format_source_with_module_items(
        "const value = 1;\nconst readValue = () => value;\nconsole.log(readValue());",
        &[],
        &[],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(!formatted.contains("readValue ="));
    assert!(formatted.contains("console.log(value);"));
}

#[test]
fn module_item_formatting_inlines_single_use_function_expression_helper() {
    let formatted = format_source_with_module_items(
        "const value = 1;\nconst readValue = function() { return value; };\nconsole.log(readValue());",
        &[],
        &[],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(!formatted.contains("readValue ="));
    assert!(formatted.contains("console.log(value);"));
}

#[test]
fn module_item_formatting_inlines_single_use_one_param_helper() {
    let formatted = format_source_with_module_items(
        "function readName(item) { return item.name; }\nconst user = { name: 'Ada' };\nconsole.log(readName(user));",
        &[],
        &[],
        Some(Path::new("modules/runtime/source-1-helpers.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(!formatted.contains("function readName"));
    assert!(formatted.contains("console.log(user.name);"));
}

#[test]
fn module_item_formatting_keeps_multi_use_tiny_return_helper() {
    let formatted = format_source_with_module_items(
        "const value = 1;\nfunction readValue() { return value; }\nconsole.log(readValue(), readValue());",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("function readValue"));
    assert!(formatted.contains("readValue(), readValue()"));
}

#[test]
fn module_item_formatting_keeps_exported_tiny_return_helper() {
    let formatted = format_source_with_module_items(
        "const value = 1;\nfunction readValue() { return value; }\nconsole.log(readValue());\nexport { readValue };",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("function readValue"));
    assert!(formatted.contains("export { readValue };"));
}

#[test]
fn module_item_formatting_keeps_this_based_tiny_return_helper() {
    let formatted = format_source_with_module_items(
        "function readThis() { return this.value; }\nconsole.log(readThis());",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("function readThis"));
    assert!(formatted.contains("readThis()"));
}

#[test]
fn module_item_formatting_keeps_one_param_helper_with_impure_arg() {
    let formatted = format_source_with_module_items(
        "function readName(item) { return item.name; }\nconsole.log(readName(makeUser()));",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("function readName"));
    assert!(formatted.contains("readName(makeUser())"));
}

#[test]
fn module_item_formatting_keeps_enhanced_tiny_helper_outside_runtime() {
    let formatted = format_source_with_module_items(
        "function compute(input) { return input * 2; }\nvar entry = compute(21);",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("function compute"));
    assert!(formatted.contains("var entry = compute(21);"));
}

#[test]
fn module_item_formatting_keeps_one_param_helper_with_object_shorthand() {
    let formatted = format_source_with_module_items(
        "const wrap = (item) => ({ item });\nconsole.log(wrap(user));",
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const wrap ="));
    assert!(formatted.contains("wrap(user)"));
}

#[test]
fn module_item_formatting_coalesces_local_named_exports() {
    let formatted = format_source_with_module_items(
        "const alpha = 1;\nconst beta = 2;\nexport { beta };\nconsole.log(alpha, beta);",
        &[],
        &[GeneratedExport::new("alpha"), GeneratedExport::new("beta")],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("export { alpha, beta };"));
    assert_eq!(formatted.matches("export {").count(), 1);
    assert!(formatted.contains("console.log(alpha, beta);"));
}

#[test]
fn module_item_formatting_keeps_alias_and_reexports_separate() {
    let formatted = format_source_with_module_items(
        "const alpha = 1;\nconst beta = 2;\nexport { beta };\nexport { beta as renamed };\nexport { gamma } from './gamma.js';",
        &[],
        &[GeneratedExport::new("alpha")],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const renamed = 2;"));
    assert!(formatted.contains("export { renamed as beta };"));
    assert!(formatted.contains("export { alpha, renamed };"));
    assert!(formatted.contains("export { gamma } from './gamma.js';"));
    assert_eq!(formatted.matches("export {").count(), 3);
}

#[test]
fn empty_module_item_formatting_emits_parseable_empty_module() {
    let formatted = format_source_with_module_items(
        "",
        &[],
        &[],
        Some(Path::new("src/empty.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("empty module should format");

    assert_eq!(formatted.trim(), "export {};");
}

#[test]
fn readability_renames_source_backed_binding_before_codegen() {
    let formatted = format_source_with_module_items_and_renames(
        "var $F1 = 1; console.log($F1); export { $F1 };",
        &[],
        &[],
        &[GeneratedRename::new("$F1", "lodashGlobalObjectInit")],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("var lodashGlobalObjectInit = 1;"));
    assert!(formatted.contains("console.log(lodashGlobalObjectInit);"));
    assert!(formatted.contains("export { lodashGlobalObjectInit as $F1 };"));
}

#[test]
fn readability_renames_every_resolved_reference_but_not_shadowed_text() {
    let formatted = format_source_with_module_items_and_renames(
        "var $F1 = 1; function outer() { console.log($F1); function inner($F1) { return $F1; } return inner; } var obj = {}; obj.$F1 = \"$F1\";",
        &[],
        &[],
        &[GeneratedRename::new("$F1", "readableValue")],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("var readableValue = 1;"));
    assert!(formatted.contains("console.log(readableValue);"));
    assert!(formatted.contains("function inner($F1)"));
    assert!(formatted.contains("return $F1;"));
    assert!(formatted.contains("obj.$F1 = '$F1';"));
}

#[test]
fn readability_renames_named_import_alias_to_imported_name() {
    let formatted = format_source_with_module_items_and_renames(
        "import { map as $F1 } from 'lodash'; console.log($F1); export { $F1 };",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { map } from 'lodash';"));
    assert!(formatted.contains("console.log(map);"));
    assert!(formatted.contains("export { map as $F1 };"));
    assert!(!formatted.contains("$F1);"));
}

#[test]
fn readability_renames_named_import_alias_skips_collisions() {
    let formatted = format_source_with_module_items_and_renames(
        "import { map as $F1 } from 'lodash'; const map = 1; console.log($F1, map);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { map as $F1 } from 'lodash';"));
    assert!(formatted.contains("const map = 1;"));
    assert!(formatted.contains("console.log($F1, map);"));
}

#[test]
fn readability_renames_generated_namespace_import_alias_from_specifier() {
    let formatted = format_source_with_module_items_and_renames(
        "const answer = __pkg_lodash_map.answer;",
        &[GeneratedImport::new("__pkg_lodash_map", "lodash/map")],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import * as lodashMap from 'lodash/map';"));
    assert!(formatted.contains("const answer = lodashMap.answer;"));
}

#[test]
fn readability_renames_namespace_import_keeps_handwritten_alias() {
    let formatted = format_source_with_module_items_and_renames(
        "import * as utilities from 'lodash'; console.log(utilities, utilities.map);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import * as utilities from 'lodash';"));
    assert!(formatted.contains("console.log(utilities, utilities.map);"));
}

#[test]
fn readability_renames_explicit_hint_takes_precedence_over_import_alias_cleanup() {
    let formatted = format_source_with_module_items_and_renames(
        "import { map as $F1 } from 'lodash'; console.log($F1);",
        &[],
        &[],
        &[GeneratedRename::new("$F1", "lodashMap")],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { map as lodashMap } from 'lodash';"));
    assert!(formatted.contains("console.log(lodashMap);"));
}

#[test]
fn readability_renames_from_export_specifier_and_uses_object_shorthand() {
    let formatted = format_source_with_module_items_and_renames(
        "const a = 1; const obj = { createClient: a }; console.log(obj); export { a as createClient };",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const createClient = 1;"));
    assert!(formatted.contains("const obj = { createClient };"));
    assert!(formatted.contains("export { createClient };"));
}

#[test]
fn readability_hint_resolver_prefers_export_name_over_later_object_property() {
    let formatted = format_source_with_module_items_and_renames(
        "const a = 1; export { a as createClient }; const obj = { internalName: a };",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const createClient = 1;"));
    assert!(formatted.contains("export { createClient };"));
    assert!(formatted.contains("const obj = { internalName: createClient };"));
    assert!(!formatted.contains("const internalName = 1;"));
}

#[test]
fn readability_hint_resolver_skips_conflicting_object_property_names() {
    let (formatted, report) = format_source_with_module_items_and_renames_with_report(
        "const a = 1; const first = { foo: a }; const second = { bar: a }; console.log(a);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const a = 1;"));
    assert!(formatted.contains("foo: a"));
    assert!(formatted.contains("bar: a"));
    assert!(formatted.contains("console.log(a);"));
    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.contains("reason=conflicting_hints"))
    );
}

#[test]
fn readability_renames_from_commonjs_export_property_and_recovers_function_declaration() {
    let formatted = format_source_with_module_items_and_renames(
        "const a = function() { return 1; }; exports.createClient = a;",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("function createClient()"));
    assert!(formatted.contains("exports.createClient = createClient;"));
    assert!(!formatted.contains("const a = function"));
}

#[test]
fn readability_renames_from_commonjs_bracket_export_property() {
    let formatted = format_source_with_module_items_and_renames(
        "const a = 1; exports['createClient'] = a;",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const createClient = 1;"));
    assert!(formatted.contains("exports['createClient'] = createClient;"));
}

#[test]
fn readability_renames_from_module_exports_object_and_uses_shorthand() {
    let formatted = format_source_with_module_items_and_renames(
        "const a = 1; module.exports = { createClient: a };",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const createClient = 1;"));
    assert!(formatted.contains("module.exports = { createClient };"));
}

#[test]
fn readability_renames_api_object_exports_and_recovers_functions() {
    let formatted = format_source_with_module_items_and_renames(
        "const a = function() { return 1; }; const b = function() { return 2; }; module.exports = { createClient: a, close: b };",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("function createClient()"));
    assert!(formatted.contains("function close()"));
    let compact = formatted.split_whitespace().collect::<String>();
    assert!(compact.contains("module.exports={createClient,close};"));
    assert!(!formatted.contains("const a = function"));
    assert!(!formatted.contains("const b = function"));
}

#[test]
fn readability_renames_from_object_define_property_getter() {
    let formatted = format_source_with_module_items_and_renames(
        "const a = 1; Object.defineProperty(exports, 'createClient', { get: function() { return a; } });",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const createClient = 1;"));
    assert!(formatted.contains("get()"));
    assert!(formatted.contains("return createClient;"));
}

#[test]
fn readability_report_records_applied_rename_and_polish() {
    let (formatted, report) = format_source_with_module_items_and_renames_with_report(
        "const a = function() { return 1; }; exports.createClient = a;",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("function createClient()"));
    assert!(
        report
            .entries
            .iter()
            .any(|entry| { entry.contains("renamed a -> createClient, source=commonjs_export") })
    );
    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.contains("recovered function declaration createClient"))
    );
}

#[test]
fn readability_polish_inlines_safe_aliases() {
    let formatted = format_source_with_module_items_and_renames(
        "const settings = loadSettings(); const alias = settings; console.log(alias);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const settings = loadSettings();"));
    assert!(formatted.contains("console.log(settings);"));
    assert!(!formatted.contains("const alias = settings;"));
}

#[test]
fn readability_usage_based_names_generated_bindings_from_initializers() {
    let formatted = format_source_with_module_items_and_renames(
        "const a = new Client(); const b = createLogger(); console.log(a, b);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const client = new Client();"));
    assert!(formatted.contains("const logger = createLogger();"));
    assert!(formatted.contains("console.log(client, logger);"));
}

#[test]
fn readability_usage_based_name_does_not_override_public_export_name() {
    let formatted = format_source_with_module_items_and_renames(
        "const a = createLogger(); export { a as createClient };",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const createClient = createLogger();"));
    assert!(formatted.contains("export { createClient };"));
    assert!(!formatted.contains("const logger = createLogger();"));
}

#[test]
fn readability_usage_based_names_keep_readable_short_bindings() {
    let formatted = format_source_with_module_items_and_renames(
        "const id = new Client(); const v1 = createLogger(); console.log(id, v1);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const id = new Client();"));
    assert!(formatted.contains("const logger = createLogger();"));
    assert!(formatted.contains("console.log(id, logger);"));
    assert!(!formatted.contains("const client = new Client();"));
}

#[test]
fn readability_polish_keeps_exported_aliases() {
    let formatted = format_source_with_module_items_and_renames(
        "const settings = 1; const alias = settings; console.log(alias); export { alias };",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const alias = settings;"));
    assert!(formatted.contains("console.log(alias);"));
    assert!(formatted.contains("export { alias };"));
}

#[test]
fn readability_polish_keeps_alias_when_source_name_is_shadowed() {
    let formatted = format_source_with_module_items_and_renames(
        "const settings = 1; const alias = settings; function f(settings) { return alias; }",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const alias = settings;"));
    assert!(formatted.contains("return alias;"));
}

#[test]
fn readability_polish_recovers_object_destructuring() {
    let (formatted, report) = format_source_with_module_items_and_renames_with_report(
        "const createClient = api.createClient; const close = api.close; console.log(createClient, close);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const { createClient, close } = api;"));
    assert!(formatted.contains("console.log(createClient, close);"));
    assert!(!formatted.contains("const close = api.close;"));
    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.contains("recovered object destructuring api"))
    );
}

#[test]
fn readability_polish_recovers_aliased_object_destructuring() {
    let formatted = format_source_with_module_items_and_renames(
        "const client = api.createClient; const close = api.close; console.log(client, close);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const { createClient: client, close } = api;"));
    assert!(formatted.contains("console.log(client, close);"));
    assert!(!formatted.contains("const client = api.createClient;"));
    assert!(!formatted.contains("const close = api.close;"));
}

#[test]
fn readability_polish_merges_and_sorts_duplicate_named_imports() {
    let formatted = format_source_with_module_items_and_renames(
        "import { z } from 'pkg'; import { a } from 'pkg'; console.log(z, a);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert_eq!(formatted.matches("from 'pkg'").count(), 1);
    assert!(formatted.contains("import { a, z } from 'pkg';"));
}

#[test]
fn readability_polish_merges_default_and_named_imports() {
    let formatted = format_source_with_module_items_and_renames(
        "import React from 'react'; import { useMemo } from 'react'; console.log(React, useMemo);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert_eq!(formatted.matches("from 'react'").count(), 1);
    assert!(formatted.contains("import React, { useMemo } from 'react';"));
    assert!(formatted.contains("console.log(React, useMemo);"));
}

#[test]
fn readability_polish_splits_safe_namespace_imports() {
    let formatted = format_source_with_module_items_and_renames(
        "import * as lodash from 'lodash'; console.log(lodash.map(items, fn), lodash.filter(items, fn));",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import { filter, map } from 'lodash';"));
    assert!(formatted.contains("console.log(map(items, fn), filter(items, fn));"));
    assert!(!formatted.contains("lodash.map"));
}

#[test]
fn readability_polish_keeps_namespace_import_when_namespace_escapes() {
    let formatted = format_source_with_module_items_and_renames(
        "import * as lodash from 'lodash'; console.log(lodash, lodash.map);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import * as lodash from 'lodash';"));
    assert!(formatted.contains("console.log(lodash, lodash.map);"));
}

#[test]
fn readability_polish_recovers_class_declaration() {
    let formatted = format_source_with_module_items_and_renames(
        "const Client = class { connect() { return 1; } }; console.log(new Client());",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("class Client"));
    assert!(formatted.contains("connect()"));
    assert!(formatted.contains("console.log(new Client());"));
    assert!(!formatted.contains("const Client = class"));
}

#[test]
fn readability_polish_recovers_object_method_shorthand() {
    let formatted = format_source_with_module_items_and_renames(
        "const api = { createClient: function() { return 1; } };",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("createClient()"));
    assert!(!formatted.contains("createClient: function"));
}

#[test]
fn readability_polish_is_idempotent_after_late_transforms() {
    let source = "\
        import * as lodash from 'lodash';\n\
        const a = function() { return lodash.map([1], x => x); };\n\
        const Client = class { connect() { return a(); } };\n\
        const api = { createClient: function() { return a(); } };\n\
        exports.createClient = a;\n\
        console.log(lodash.filter([1], Boolean), Client, api);";
    let first = format_source_with_module_items_and_renames(
        source,
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");
    let second = format_source_with_module_items_and_renames(
        first.as_str(),
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format again");

    assert_eq!(second, first);
    assert!(first.contains("import { filter, map } from 'lodash';"));
    assert!(first.contains("function createClient()"));
    assert!(first.contains("class Client"));
    assert!(first.contains("createClient()"));
}

#[test]
fn readability_polish_recovers_function_declaration_when_not_used_before_declaration() {
    let formatted = format_source_with_module_items_and_renames(
        "const createClient = function() { return 1; }; console.log(createClient());",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("function createClient()"));
    assert!(formatted.contains("console.log(createClient());"));
}

#[test]
fn readability_polish_does_not_recover_hoisted_function_declaration() {
    let formatted = format_source_with_module_items_and_renames(
        "console.log(createClient); const createClient = function() { return 1; };",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("const createClient = function()"));
}

#[test]
fn readability_renames_skip_root_scope_collisions() {
    let formatted = format_source_with_module_items_and_renames(
        "var a = 1; var settings = 2; console.log(a, settings);",
        &[],
        &[],
        &[GeneratedRename::new("a", "settings")],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("var a = 1;"));
    assert!(formatted.contains("var settings = 2;"));
    assert!(formatted.contains("console.log(a, settings);"));
}

#[test]
fn readability_renames_skip_generated_import_collisions() {
    let formatted = format_source_with_module_items_and_renames(
        "var a = 1; console.log(a);",
        &[GeneratedImport::new("settings", "pkg")],
        &[],
        &[GeneratedRename::new("a", "settings")],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("import * as settings from 'pkg';"));
    assert!(formatted.contains("var a = 1;"));
    assert!(formatted.contains("console.log(a);"));
}

#[test]
fn readability_renames_skip_duplicate_targets() {
    let formatted = format_source_with_module_items_and_renames(
        "var a = 1; var b = 2; console.log(a, b);",
        &[],
        &[],
        &[
            GeneratedRename::new("a", "settings"),
            GeneratedRename::new("b", "settings"),
        ],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("var settings = 1;"));
    assert!(formatted.contains("var b = 2;"));
    assert!(formatted.contains("console.log(settings, b);"));
}

#[test]
fn readability_renames_skip_names_that_would_capture_globals() {
    let formatted = format_source_with_module_items_and_renames(
        "var a = 1; function f() { return settings; }",
        &[],
        &[],
        &[GeneratedRename::new("a", "settings")],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("var a = 1;"));
    assert!(formatted.contains("return settings;"));
}

#[test]
fn emit_safety_renames_strict_reserved_bindings_before_esm_output() {
    let formatted = format_source_with_module_items_and_renames(
        "var package = 1; function read() { var private = package + 1; return private; } console.log(package, read());",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("var _package = 1;"));
    assert!(formatted.contains("var _private = _package + 1;"));
    assert!(formatted.contains("return _private;"));
    assert!(formatted.contains("console.log(_package, read());"));
    assert!(!formatted.contains("var package"));
    assert!(!formatted.contains("var private"));
}

#[test]
fn emit_safety_renames_avoid_existing_binding_collisions() {
    let formatted = format_source_with_module_items_and_renames(
        "var _package = 1; var package = 2; console.log(_package, package);",
        &[],
        &[],
        &[],
        Some(Path::new("src/index.ts")),
        ParseGoal::TypeScript,
        CompilerLowering::None,
    )
    .expect("fixture should format");

    assert!(formatted.contains("var _package = 1;"));
    assert!(formatted.contains("_package2 = 2"));
    assert!(formatted.contains("console.log(_package, _package2);"));
}

#[test]
fn pipeline_normalization_accepts_commonjs_bin_sources() {
    let normalized = normalize_source_for_pipeline(
        "if (require.main === module) {\n  return;\n}\nmodule.exports = {};\n",
        Some(Path::new("bin/which.js")),
    )
    .expect("commonjs package source should normalize");

    assert!(normalized.contains("module.exports"));
}

#[test]
fn sanitizes_package_and_minifier_fragments_into_identifiers() {
    assert_eq!(sanitize_identifier("@smithy/XY7"), "_smithy_XY7");
    assert_eq!(sanitize_identifier("9patch-name"), "_9patch_name");
    assert_eq!(sanitize_identifier("class"), "_class");
    assert_eq!(sanitize_identifier("package"), "_package");
    assert_eq!(sanitize_identifier("private"), "_private");
    assert_eq!(sanitize_identifier("arguments"), "_arguments");
}

fn binding_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_string()).collect()
}

#[test]
fn classifies_top_level_reference_in_statement() {
    let source = "import { foo } from './x.js';\nconst y = foo;";
    let scope = classify_import_usage_scope(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(scope.get("foo"), Some(&ImportUsageScope::TopLevel));
}

#[test]
fn classifies_reference_inside_function_body_as_nested() {
    let source = "import { foo } from './x.js';\nexport function call() { return foo(); }";
    let scope = classify_import_usage_scope(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(scope.get("foo"), Some(&ImportUsageScope::NestedOnly));
}

#[test]
fn classifies_reference_inside_arrow_body_as_nested() {
    let source = "import { foo } from './x.js';\nconst trigger = () => foo();";
    let scope = classify_import_usage_scope(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(scope.get("foo"), Some(&ImportUsageScope::NestedOnly));
}

#[test]
fn classifies_reference_inside_method_body_as_nested() {
    let source = "import { foo } from './x.js';\nclass S { method() { return foo; } }";
    let scope = classify_import_usage_scope(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(scope.get("foo"), Some(&ImportUsageScope::NestedOnly));
}

#[test]
fn classifies_reference_inside_class_static_block_as_top_level() {
    // `static { ... }` runs at class-declaration time. If the class
    // is declared at module top level, the static block code is on
    // the module-load critical path.
    let source = "import { foo } from './x.js';\nclass S { static { foo(); } }";
    let scope = classify_import_usage_scope(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(scope.get("foo"), Some(&ImportUsageScope::TopLevel));
}

#[test]
fn classifies_reference_inside_class_field_initializer_as_top_level() {
    // Class field initializers in `class C { x = foo; }` run at
    // `new C()` time, not class-decl time. But the simple visitor
    // can't distinguish "runs at instantiation" from "runs at decl"
    // for instance fields — and instance fields conservatively
    // appearing TopLevel keeps us safe (we'll keep more thunks
    // lazy rather than fewer). Static initializers, on the other
    // hand, run at class declaration and are correctly TopLevel.
    let source = "import { foo } from './x.js';\nclass S { static defaultFoo = foo; }";
    let scope = classify_import_usage_scope(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(scope.get("foo"), Some(&ImportUsageScope::TopLevel));
}

#[test]
fn unreferenced_bindings_default_to_nested_only() {
    // No occurrence of `foo` anywhere — zero references vacuously
    // satisfies "every reference is nested-only".
    let source = "import { foo } from './x.js';\nconst y = 42;";
    let scope = classify_import_usage_scope(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(scope.get("foo"), Some(&ImportUsageScope::NestedOnly));
}

#[test]
#[should_panic(expected = "import usage scope classification requires parseable source")]
fn import_usage_scope_rejects_unparseable_source() {
    let _ = classify_import_usage_scope(
        "function entry(",
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
}

#[test]
fn promotes_to_top_level_on_first_top_level_occurrence() {
    // `foo` appears both inside a function (nested) and at the top
    // level (the expression statement `foo;`). The classification
    // must reflect the most restrictive observation — TopLevel.
    let source = "import { foo } from './x.js';\nfunction call() { return foo(); }\nfoo;";
    let scope = classify_import_usage_scope(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(scope.get("foo"), Some(&ImportUsageScope::TopLevel));
}

#[test]
fn ignores_property_keys_named_same_as_target_binding() {
    // `obj.foo` and `{ foo: 1 }` are property-key uses, not
    // references to a binding named `foo`. The visitor must not
    // misclassify them.
    let source = concat!(
        "import { foo } from './x.js';\n",
        "const obj = { foo: 1 };\n",
        "console.log(obj.foo);\n",
    );
    let scope = classify_import_usage_scope(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(scope.get("foo"), Some(&ImportUsageScope::NestedOnly));
}

#[test]
fn verifies_immediate_call_form_when_every_reference_is_x_zero_args() {
    let source = concat!(
        "import { foo } from './x.js';\n",
        "console.log(foo());\n",
        "function call() { return foo(); }\n",
    );
    let result = verify_only_immediate_call_references(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(result.get("foo"), Some(&true));
}

#[test]
fn rejects_immediate_call_form_when_binding_used_as_value() {
    // `register(foo)` passes `foo` as a value, not invoking it.
    let source = concat!(
        "import { foo } from './x.js';\n",
        "register(foo);\n",
        "foo();\n",
    );
    let result = verify_only_immediate_call_references(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(result.get("foo"), Some(&false));
}

#[test]
fn rejects_immediate_call_form_when_called_with_arguments() {
    // `foo(1)` is calling foo with an argument — not the zero-arg
    // thunk-call pattern. The binding is being used as a callable
    // value directly; eagerifying would change the call semantics.
    let source = concat!("import { foo } from './x.js';\n", "foo();\n", "foo(1);\n",);
    let result = verify_only_immediate_call_references(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(result.get("foo"), Some(&false));
}

#[test]
fn rejects_immediate_call_form_when_constructed() {
    let source = concat!("import { foo } from './x.js';\n", "new foo();\n",);
    let result = verify_only_immediate_call_references(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(result.get("foo"), Some(&false));
}

#[test]
fn rejects_immediate_call_form_on_typeof_check() {
    let source = concat!(
        "import { foo } from './x.js';\n",
        "if (typeof foo === 'function') foo();\n",
    );
    let result = verify_only_immediate_call_references(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(result.get("foo"), Some(&false));
}

#[test]
fn rejects_immediate_call_form_on_chained_call_result_use() {
    // `foo()` is followed by `.bar` access. The first call is the
    // expected zero-arg form, but `.bar` access on its result is
    // still a separate operation. Identifier count: 1 (foo).
    // Call count: 1 (foo()). Result: total == calls → true.
    //
    // However if there's ALSO `foo` used elsewhere as a value, the
    // result flips to false. This test confirms the chained form
    // alone is still treated as rewritable.
    let source = concat!(
        "import { foo } from './x.js';\n",
        "const value = foo().bar;\n",
        "console.log(foo().baz);\n",
    );
    let result = verify_only_immediate_call_references(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(result.get("foo"), Some(&true));
}

#[test]
fn vacuously_safe_when_binding_is_never_referenced() {
    let source = "import { foo } from './x.js';\nconst y = 42;";
    let result = verify_only_immediate_call_references(
        source,
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(result.get("foo"), Some(&true));
}

#[test]
#[should_panic(expected = "call-reference verification requires parseable source")]
fn immediate_call_reference_verification_rejects_unparseable_source() {
    let _ = verify_only_immediate_call_references(
        "function entry(",
        &binding_set(&["foo"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
}

#[test]
fn lazy_body_classifier_extracts_direct_module_exports_value() {
    let value = extract_lazy_module_eager_value(
        "module.exports = 42;",
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value.as_deref(), Some("42"));
}

#[test]
fn lazy_body_classifier_extracts_chain_assignment_rightmost_value() {
    // The chain `module.exports = A = class Foo {}` writes the
    // class expression to both the local `A` and `module.exports`.
    // The classifier extracts the rightmost pure expression; the
    // intermediate locals are discarded with their `var` declaration.
    let value = extract_lazy_module_eager_value(
        "var A;\nmodule.exports = A = class Foo { constructor() {} };",
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value.as_deref(), Some("class Foo { constructor() {} }"));
}

#[test]
fn lazy_body_classifier_unwraps_iife_call_wrapper() {
    // The pattern `(function() { body }).call(this)` is a common
    // CJS shape that hides a simple `module.exports = X`
    // assignment behind an IIFE. The classifier recursively
    // descends into the IIFE body.
    let body = "(function() { var A; module.exports = A = class { hello() { return 1; } }; }).call(exports);";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value.as_deref(), Some("class { hello() { return 1; } }"));
}

#[test]
fn lazy_body_classifier_unwraps_arrow_iife_wrapper() {
    let body = "(() => { module.exports = { ok: true }; })();";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value.as_deref(), Some("{ ok: true }"));
}

#[test]
fn lazy_body_classifier_handles_object_define_property_with_pure_value() {
    let body = "Object.defineProperty(exports, \"value\", { value: 99, configurable: true });";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value.as_deref(), Some("{ value: 99 }"));
}

#[test]
fn lazy_body_classifier_accepts_return_for_lazy_value_shape() {
    // `lazyValue(() => { return PURE; })` — for lazy-value
    // (no `module` parameter), the body is a `return` of a pure
    // expression.
    let value = extract_lazy_module_eager_value(
        "return { primary: '#abc', secondary: '#def' };",
        "",
        None,
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(
        value.as_deref(),
        Some("{ primary: '#abc', secondary: '#def' }")
    );
}

#[test]
fn lazy_body_classifier_rejects_function_call_in_body() {
    // The body has a top-level call to `initSetup()` — could have
    // any side effect, can't hoist to module load.
    let body = "initSetup();\nmodule.exports = 42;";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value, None);
}

#[test]
fn lazy_body_classifier_rejects_assignment_to_other_target() {
    // Assignment to a non-`module.exports`/`exports.k` target — could
    // have side effects on globals or other observable state.
    let body = "globalThis.config = 99;\nmodule.exports = 42;";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value, None);
}

#[test]
fn lazy_body_classifier_rejects_multiple_module_exports_assignments() {
    // Two separate `module.exports = ...` writes — the final value
    // would depend on evaluation order, which conservative
    // classification refuses to pick from.
    let body = "module.exports = 1; module.exports = 2;";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value, None);
}

#[test]
fn lazy_body_classifier_collapses_multi_key_exports_to_object_literal() {
    let body = "exports.parse = function(s) { return s; };\nexports.stringify = function(o) { return o; };";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(
        value.as_deref(),
        Some("{ parse: function(s) { return s; }, stringify: function(o) { return o; } }")
    );
}

#[test]
fn lazy_body_classifier_rejects_impure_chain_value() {
    // `module.exports = A = computeStuff()` — the final value is
    // a function call, which can have side effects. Reject.
    let body = "var A;\nmodule.exports = A = computeStuff();";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value, None);
}

#[test]
fn lazy_body_classifier_accepts_reverts_setter_call_with_pure_arg() {
    // Setter call alongside an exports write — common in CJS
    // wrappers where some helper bindings are set as side effects
    // before the main exports value. Phase 8e folds the setter
    // into a leading comma expression so the side effect still
    // runs at module load.
    let body = "__reverts_set_helper(42); module.exports = 'value';";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(
        value.as_deref(),
        Some("(__reverts_set_helper(42), 'value')")
    );
}

#[test]
fn lazy_body_classifier_accepts_bare_identifier_statement() {
    // `x;` as a standalone expression statement is a no-op the
    // bundler emits to keep imports from being tree-shaken. Drop
    // it on collapse.
    let body = "bareImport;\nmodule.exports = 1;";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value.as_deref(), Some("1"));
}

#[test]
fn lazy_body_classifier_collapses_exports_property_assignments() {
    let body = "exports.foo = 1; exports.bar = 2;";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );

    assert_eq!(value.as_deref(), Some("{ foo: 1, bar: 2 }"));
}

#[test]
fn lazy_body_classifier_emits_init_only_lazy_module_as_empty_exports() {
    // A `lazyModule` body that only invokes setters (no
    // `module.exports = ...` write) eagerifies to `({}, ...,
    // emptyExports)` where emptyExports is the wrapper's default
    // — observers of `X()` previously got `{}` (the untouched
    // exports object), so the rewrite preserves that.
    let body = "__reverts_set_a(1); __reverts_set_b(2);";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(
        value.as_deref(),
        Some("(__reverts_set_a(1), __reverts_set_b(2), {})")
    );
}

#[test]
fn lazy_body_classifier_emits_init_only_lazy_value_as_void_zero() {
    // For `lazyValue` bodies (no module param), an init-only
    // body returned `undefined` originally; the eagerified form
    // is `(setters..., void 0)`.
    let body = "__reverts_set_a(1);\n__reverts_set_b(2);";
    let value = extract_lazy_module_eager_value(
        body,
        "",
        None,
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(
        value.as_deref(),
        Some("(__reverts_set_a(1), __reverts_set_b(2), void 0)")
    );
}

#[test]
fn lazy_body_classifier_accepts_comma_separated_setter_sequence() {
    // esbuild commonly emits multi-setter init as one statement
    // joined by commas: `setterA(1), setterB(2), setterC(3);` —
    // this is a SequenceExpression in the AST. Phase 8e walks
    // each element and accepts the whole sequence when every
    // comma-separated call is a setter (or a bare ident).
    let body = "__reverts_set_a(1), __reverts_set_b(true), __reverts_set_c({ key: 'v' });";
    let value = extract_lazy_module_eager_value(
        body,
        "",
        None,
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    // Note: the comma-separated setters are pushed individually
    // into the prologue, so the emitted comma expression flattens
    // them: `(setter_a(...), setter_b(...), setter_c(...), void 0)`.
    assert_eq!(
        value.as_deref(),
        Some("(__reverts_set_a(1), __reverts_set_b(true), __reverts_set_c({ key: 'v' }), void 0)")
    );
}

#[test]
fn lazy_body_classifier_rejects_setter_with_function_call_arg() {
    // `__reverts_set_X(otherThunk())` — the argument is a function
    // call, which could have side effects we can't see. The
    // existing impure-call rejection is the safety floor for
    // Phase 8e; inter-procedural classification is a separate
    // future pass.
    let body = "__reverts_set_a(loadData()); module.exports = 1;";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value, None);
}

#[test]
fn lazy_body_classifier_rejects_non_setter_call_in_body() {
    let body = "initSomething();\nmodule.exports = 1;";
    let value = extract_lazy_module_eager_value(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(value, None);
}

#[test]
fn classify_lazy_body_returns_deps_for_zero_arg_thunk_calls() {
    // Body has bare zero-arg calls to imported thunks alongside an
    // exports write. These become inter-procedural dependencies —
    // the fixpoint resolves them; the value still composes.
    let body = "initOne(); initTwo(); module.exports = 42;";
    let result = classify_lazy_module_body(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    match result {
        LazyBodyClassification::EagerWithDeps { value, call_deps } => {
            // Thunk calls are NOT in the prologue — they're handled
            // by their producer's eagerification.
            assert_eq!(value, "42");
            assert!(call_deps.contains("initOne"));
            assert!(call_deps.contains("initTwo"));
            assert_eq!(call_deps.len(), 2);
        }
        other => panic!("expected EagerWithDeps, got {other:?}"),
    }
}

#[test]
fn classify_lazy_body_treats_setter_calls_alongside_thunk_deps_correctly() {
    // Mix of setter calls (go into prologue, run at module load)
    // and zero-arg thunk calls (become deps, handled by their own
    // eagerification). The value composes the setters + the
    // captured exports write.
    let body = "thunkA(); __reverts_set_foo(1); thunkB(); module.exports = bar;";
    // module.exports = bar — `bar` is an identifier (not pure) so
    // captured_value rejects → Impure overall.
    let result = classify_lazy_module_body(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(result, LazyBodyClassification::Impure);
}

#[test]
fn classify_lazy_body_thunk_only_init_returns_deps_with_empty_exports() {
    // No exports write, no return — just thunk calls. lazyModule
    // bodies yield `{}` (the wrapper's empty exports object).
    let body = "thunkA();\nthunkB();";
    let result = classify_lazy_module_body(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    match result {
        LazyBodyClassification::EagerWithDeps { value, call_deps } => {
            assert_eq!(value, "{}");
            assert_eq!(call_deps.len(), 2);
        }
        other => panic!("expected EagerWithDeps, got {other:?}"),
    }
}

#[test]
fn classify_lazy_body_rejects_call_with_arguments_as_unknown_effect() {
    // `foo(1)` — call with an argument is NOT a zero-arg thunk
    // invocation; it could be calling a regular function with side
    // effects we can't classify. Stay impure.
    let body = "foo(1); module.exports = 42;";
    let result = classify_lazy_module_body(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(result, LazyBodyClassification::Impure);
}

#[test]
fn classify_lazy_body_eager_when_body_has_no_calls() {
    let body = "module.exports = { a: 1, b: 2 };";
    let result = classify_lazy_module_body(
        body,
        "exports",
        Some("module"),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    match result {
        LazyBodyClassification::Eager { value } => {
            assert_eq!(value, "{ a: 1, b: 2 }");
        }
        other => panic!("expected Eager, got {other:?}"),
    }
}

#[test]
fn classifies_multiple_bindings_independently() {
    let source = concat!(
        "import { eager, lazy } from './x.js';\n",
        "const result = eager;\n",
        "export function trigger() { return lazy(); }\n",
    );
    let scope = classify_import_usage_scope(
        source,
        &binding_set(&["eager", "lazy"]),
        Some(Path::new("entry.ts")),
        ParseGoal::TypeScript,
    );
    assert_eq!(scope.get("eager"), Some(&ImportUsageScope::TopLevel));
    assert_eq!(scope.get("lazy"), Some(&ImportUsageScope::NestedOnly));
}

#[test]
fn lazy_value_sub_snippets_slices_arrow_body_statements() {
    let source = "var X = lazyValue(() => {\n\
                  \tvar a = 1;\n\
                  \tvar b = a + 1;\n\
                  \tfunction c() { return a; }\n\
                  });";

    let slices = lazy_value_sub_snippets(source, None, ParseGoal::TypeScript)
        .expect("recognised lazyValue shape");

    assert_eq!(slices.len(), 3, "{slices:?}");
    assert_eq!(slices[0].kind, TopLevelStatementKind::Variable);
    assert_eq!(slices[0].bindings, vec!["a".to_string()]);
    assert_eq!(slices[1].kind, TopLevelStatementKind::Variable);
    assert_eq!(slices[1].bindings, vec!["b".to_string()]);
    assert_eq!(slices[2].kind, TopLevelStatementKind::Function);
    assert_eq!(slices[2].bindings, vec!["c".to_string()]);
    assert!(slices[0].source.contains("var a = 1;"));
    assert!(slices[1].source.contains("var b = a + 1;"));
    assert!(slices[2].source.contains("function c()"));
}

#[test]
fn lazy_value_sub_snippets_returns_none_for_non_lazy_shape() {
    assert!(
        lazy_value_sub_snippets("var X = 1;", None, ParseGoal::TypeScript).is_none(),
        "plain var declaration is not a lazyValue"
    );
    assert!(
        lazy_value_sub_snippets("var X = lazyValue(() => 42);", None, ParseGoal::TypeScript)
            .is_none(),
        "expression-only arrow body is not slice-able"
    );
    assert!(
        lazy_value_sub_snippets(
            "var X = otherFn(() => { var a = 1; });",
            None,
            ParseGoal::TypeScript
        )
        .is_none(),
        "non-lazyValue callee is not a lazy block"
    );
}
