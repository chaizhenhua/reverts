use oxc_ast::Visit;
use oxc_ast::ast::{
    Argument, AssignmentExpression, AssignmentTarget, BindingPatternKind, CallExpression,
    Expression, ObjectPropertyKind, Program, PropertyKey, Statement, VariableDeclaration,
    VariableDeclarator,
};
use oxc_ast::visit::walk::walk_assignment_expression;
use oxc_span::GetSpan;
use reverts_ir::{ByteRange, ModuleId};
use std::collections::BTreeSet;

use crate::detectors::esbuild_helpers::discover_aliases;
use crate::inner_module::{BundlerKind, InnerModule};

/// Recognise esbuild's `__commonJS` registration forms — both the
/// un-minified `__commonJS({"path": fn, ...})` map and the minified
/// `var <name> = <alias>(fn)` per-module call.
///
/// The detector first discovers local names bound to the `__commonJS` helper
/// by AST shape (see [`discover_aliases`]) then matches every call/assignment
/// that uses one of those proven helper names.
#[must_use]
pub fn detect_commonjs(
    source: &str,
    program: &Program<'_>,
    parent_module_id: ModuleId,
) -> Vec<InnerModule> {
    let aliases = discover_aliases(program);
    let mut out = detect_named_registry(program, parent_module_id, &aliases.commonjs);
    out.extend(detect_var_assignment_modules(
        source,
        program,
        parent_module_id,
        &aliases.commonjs,
    ));
    out
}

/// Recognise esbuild's `__esm` registration forms — both the
/// un-minified `__esm({"path": fn, ...})` map and the minified
/// `var <name> = <alias>(fn)` per-module call.
#[must_use]
pub fn detect_esm(
    source: &str,
    program: &Program<'_>,
    parent_module_id: ModuleId,
) -> Vec<InnerModule> {
    let aliases = discover_aliases(program);
    let mut out = detect_named_registry(program, parent_module_id, &aliases.esm);
    out.extend(detect_var_assignment_modules(
        source,
        program,
        parent_module_id,
        &aliases.esm,
    ));
    out
}

/// Un-minified form: `<callee>({"path1": fn, "path2": fn, …})`.
/// Each property of the object literal becomes one `InnerModule`.
fn detect_named_registry(
    program: &Program<'_>,
    parent_module_id: ModuleId,
    callee_names: &[String],
) -> Vec<InnerModule> {
    let mut out = Vec::new();
    let mut visitor = NamedRegistryVisitor {
        out: &mut out,
        parent_module_id,
        callee_names,
    };
    visitor.visit_program(program);
    out
}

/// Minified form: top-level `var <name> = <alias>(<arrow>)`. Each such
/// declaration becomes one `InnerModule` whose `body_span` covers the
/// arrow's body. The source path key is lost during minification, so
/// `source_path_hint` is `None` and `virtual_id` derives from the
/// binding name (`esbuild:<name>`).
fn detect_var_assignment_modules(
    source: &str,
    program: &Program<'_>,
    parent_module_id: ModuleId,
    aliases: &[String],
) -> Vec<InnerModule> {
    if aliases.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for stmt in &program.body {
        let Statement::VariableDeclaration(vd) = stmt else {
            continue;
        };
        // esbuild scope-hoisting declares a module's top-level vars in the same
        // `var` statement as its init handle (`var a,b,X=helper(()=>{...})`),
        // with only initializer code in the arrow body. When the statement has
        // exactly one handle declarator we own the WHOLE statement, so the
        // hoisted sibling declarators and the handle become definitions rather
        // than free variables.
        let handle_count = vd
            .declarations
            .iter()
            .filter(|decl| is_helper_init_declarator(decl, aliases))
            .count();
        let statement_span = ByteRange::new(vd.span().start, vd.span().end);
        // Multiple handles in one statement (`var a,X=helper(()=>{...}),b,Y=...`)
        // have no single contiguous owning unit. Reconstruct each handle into
        // its own `var <hoisted-it-writes>, X=helper(()=>{...});` statement
        // (synthetic source) so each handle name becomes a real definition /
        // export — otherwise cross-module `X()`/`Y()` calls dangle.
        if handle_count > 1
            && let Some(reconstructed) = reconstruct_multi_handle_statement(source, vd, aliases)
        {
            for (handle_name, synthetic) in reconstructed {
                out.push(InnerModule {
                    virtual_id: format!("esbuild:{handle_name}"),
                    body_span: statement_span,
                    bundler: BundlerKind::Esbuild,
                    source_path_hint: None,
                    parent_module_id,
                    synthetic_source: Some(synthetic),
                });
            }
            continue;
        }
        // Multi-handle statements with ambiguous hoisted-var attribution (a bare
        // var written by >1 handle) fall through to per-arrow-body spans below
        // (a measured residual; never malformed output).
        for decl in &vd.declarations {
            let BindingPatternKind::BindingIdentifier(binding) = &decl.id.kind else {
                continue;
            };
            let Some(Expression::CallExpression(call)) = decl.init.as_ref() else {
                continue;
            };
            let Expression::Identifier(callee_id) = &call.callee else {
                continue;
            };
            if !aliases.iter().any(|a| a == callee_id.name.as_str()) {
                continue;
            }
            let Some(arg) = call.arguments.first() else {
                continue;
            };
            let arrow_body_span = match arg {
                Argument::ArrowFunctionExpression(a) => {
                    let s = a.body.span();
                    ByteRange::new(s.start, s.end)
                }
                Argument::FunctionExpression(f) => {
                    let Some(body) = f.body.as_ref() else {
                        continue;
                    };
                    let s = body.span();
                    ByteRange::new(s.start, s.end)
                }
                _ => continue,
            };
            let body_span = if handle_count == 1 {
                statement_span
            } else {
                arrow_body_span
            };
            out.push(InnerModule {
                virtual_id: format!("esbuild:{}", binding.name.as_str()),
                body_span,
                bundler: BundlerKind::Esbuild,
                source_path_hint: None,
                parent_module_id,
                synthetic_source: None,
            });
        }
    }
    out
}

/// Reconstruct a multi-handle `var` statement into one synthetic single-handle
/// statement per handle. Each bare (init-less) hoisted declarator is attached
/// to the handle whose arrow body WRITES it; a bare var written by no handle
/// goes to the nearest following (else preceding) handle — esbuild emits a
/// module's hoisted vars adjacent to its init. Returns `None` (caller falls
/// back to per-arrow-body spans) when a bare var is written by more than one
/// handle (genuinely shared mutable state — ambiguous, never guessed) or the
/// statement contains a declarator that is neither bare nor a helper init.
///
/// Output entries are `(handle_name, "var <bares>, <handle_declarator>;")`,
/// where `<handle_declarator>` is the original `X=helper(()=>{...})` source
/// slice so the synthetic statement lowers exactly like a real single-handle
/// module.
fn reconstruct_multi_handle_statement(
    source: &str,
    vd: &VariableDeclaration<'_>,
    aliases: &[String],
) -> Option<Vec<(String, String)>> {
    // Classify declarators in source order. A `Handle` is a helper-init
    // declarator (`X=helper(()=>{...})`); every other declarator is a hoisted
    // `Member` — a bare var (`a`) or a co-hoisted definition (`iNe="..."`,
    // `HBr=e=>{...}`) — carried verbatim into its owning handle's statement.
    enum Slot<'s> {
        Member {
            name: &'s str,
            decl_text: &'s str,
            is_bare: bool,
        },
        Handle {
            name: &'s str,
            decl_text: &'s str,
        },
    }
    let mut slots: Vec<Slot> = Vec::new();
    let mut handle_order: Vec<usize> = Vec::new(); // indices into `slots` that are handles
    for decl in &vd.declarations {
        let BindingPatternKind::BindingIdentifier(binding) = &decl.id.kind else {
            return None; // destructuring binding — not the hoist+handle shape
        };
        let name = binding.name.as_str();
        let span = decl.span();
        let decl_text = source.get(span.start as usize..span.end as usize)?;
        if is_helper_init_declarator(decl, aliases) {
            handle_order.push(slots.len());
            slots.push(Slot::Handle { name, decl_text });
        } else {
            slots.push(Slot::Member {
                name,
                decl_text,
                is_bare: decl.init.is_none(),
            });
        }
    }
    if handle_order.len() < 2 {
        return None;
    }

    // Which identifiers each handle's arrow body writes (its module's hoisted
    // vars), parallel to `handle_order`.
    let writes_by_handle: Vec<BTreeSet<String>> = handle_order
        .iter()
        .map(|&slot_idx| handle_written_identifiers(&vd.declarations[slot_idx]))
        .collect();

    // Attribute each hoisted member (by slot index) to one handle (slot index):
    // a bare var goes to the unique handle that writes it; an initialized member
    // (or a bare var written by none) goes to the nearest following handle —
    // esbuild emits a module's hoisted defs adjacent to its init.
    let mut members_for_handle: std::collections::BTreeMap<usize, Vec<&str>> =
        std::collections::BTreeMap::new();
    for (slot_idx, slot) in slots.iter().enumerate() {
        let Slot::Member {
            name,
            decl_text,
            is_bare,
        } = slot
        else {
            continue;
        };
        let owner = if *is_bare {
            let writers: Vec<usize> = handle_order
                .iter()
                .enumerate()
                .filter(|(handle_pos, _slot)| writes_by_handle[*handle_pos].contains(*name))
                .map(|(_handle_pos, &slot)| slot)
                .collect();
            match writers.as_slice() {
                [single] => *single,
                [] => nearest_handle(&handle_order, slot_idx)?,
                _ => return None, // bare var written by >1 handle: ambiguous shared state
            }
        } else {
            nearest_handle(&handle_order, slot_idx)?
        };
        members_for_handle.entry(owner).or_default().push(decl_text);
    }

    // Emit one synthetic statement per handle, in source order. Each declarator
    // (member + handle) gets its OWN `var` statement so the helper-init handle
    // sits in single-declarator form — the planner's helper-rename matches
    // `var X=helper(...)` only when `var` is immediately before the binding,
    // and would miss a continuation-position `<bare>, ..., X=helper(...)`.
    let mut out = Vec::new();
    for &handle_slot in &handle_order {
        let Slot::Handle { name, decl_text } = &slots[handle_slot] else {
            unreachable!("handle_order only holds handle slots");
        };
        let members = members_for_handle.remove(&handle_slot).unwrap_or_default();
        let mut synthetic = String::new();
        for member_text in &members {
            synthetic.push_str("var ");
            synthetic.push_str(member_text);
            synthetic.push_str("; ");
        }
        synthetic.push_str("var ");
        synthetic.push_str(decl_text);
        synthetic.push(';');
        out.push(((*name).to_string(), synthetic));
    }
    Some(out)
}

/// The nearest handle slot index to `bare_idx`: the first handle declared after
/// it, else the last handle declared before it.
fn nearest_handle(handle_order: &[usize], bare_idx: usize) -> Option<usize> {
    handle_order
        .iter()
        .copied()
        .find(|&h| h > bare_idx)
        .or_else(|| handle_order.iter().copied().rfind(|&h| h < bare_idx))
}

/// Identifier names that are assignment / update targets anywhere inside a
/// handle declarator's arrow (or function) body — i.e. the hoisted module-scope
/// vars this module initializes.
fn handle_written_identifiers(decl: &VariableDeclarator<'_>) -> BTreeSet<String> {
    let mut visitor = WriteTargetVisitor {
        writes: BTreeSet::new(),
    };
    if let Some(Expression::CallExpression(call)) = decl.init.as_ref()
        && let Some(arg) = call.arguments.first()
    {
        match arg {
            Argument::ArrowFunctionExpression(a) => visitor.visit_function_body(&a.body),
            Argument::FunctionExpression(f) => {
                if let Some(body) = f.body.as_ref() {
                    visitor.visit_function_body(body);
                }
            }
            _ => {}
        }
    }
    visitor.writes
}

struct WriteTargetVisitor {
    writes: BTreeSet<String>,
}

impl<'a> Visit<'a> for WriteTargetVisitor {
    fn visit_assignment_expression(&mut self, expression: &AssignmentExpression<'a>) {
        if let AssignmentTarget::AssignmentTargetIdentifier(id) = &expression.left {
            self.writes.insert(id.name.as_str().to_string());
        }
        walk_assignment_expression(self, expression);
    }
}

/// A `var` declarator whose initializer is a call to a proven `__commonJS` /
/// `__esm` helper alias — i.e. an esbuild module init handle.
fn is_helper_init_declarator(decl: &VariableDeclarator<'_>, aliases: &[String]) -> bool {
    let BindingPatternKind::BindingIdentifier(_) = &decl.id.kind else {
        return false;
    };
    let Some(Expression::CallExpression(call)) = decl.init.as_ref() else {
        return false;
    };
    let Expression::Identifier(callee_id) = &call.callee else {
        return false;
    };
    aliases.iter().any(|a| a == callee_id.name.as_str())
}

struct NamedRegistryVisitor<'a, 'n> {
    out: &'a mut Vec<InnerModule>,
    parent_module_id: ModuleId,
    callee_names: &'n [String],
}

impl<'a> Visit<'a> for NamedRegistryVisitor<'_, '_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::Identifier(callee) = &call.callee
            && self.callee_names.iter().any(|n| n == callee.name.as_str())
            && let Some(Argument::ObjectExpression(obj)) = call.arguments.first()
        {
            for prop in &obj.properties {
                let ObjectPropertyKind::ObjectProperty(p) = prop else {
                    continue;
                };
                let key_text = match &p.key {
                    PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
                    PropertyKey::StringLiteral(s) => s.value.as_str().to_string(),
                    _ => continue,
                };
                let body_span = match &p.value {
                    Expression::ArrowFunctionExpression(a) => {
                        let s = a.body.span();
                        ByteRange::new(s.start, s.end)
                    }
                    Expression::FunctionExpression(f) => {
                        let Some(body) = f.body.as_ref() else {
                            continue;
                        };
                        let s = body.span();
                        ByteRange::new(s.start, s.end)
                    }
                    _ => continue,
                };
                self.out.push(InnerModule {
                    virtual_id: format!("esbuild:{}", key_text),
                    body_span,
                    bundler: BundlerKind::Esbuild,
                    source_path_hint: Some(key_text),
                    parent_module_id: self.parent_module_id,
                    synthetic_source: None,
                });
            }
        }
        oxc_ast::visit::walk::walk_call_expression(self, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn extract(src: &str) -> Vec<InnerModule> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        detect_commonjs(src, &parsed.program, ModuleId(99))
    }

    fn extract_esm(src: &str) -> Vec<InnerModule> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        detect_esm(src, &parsed.program, ModuleId(99))
    }

    #[test]
    fn detect_commonjs_extracts_arrow_module_body() {
        let src = r#"
            var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
            var x = __commonJS({
                "node_modules/lodash/index.js": (exports, module) => {
                    module.exports = { map: function () {} };
                }
            });
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 1);
        let m = &modules[0];
        assert_eq!(m.bundler, BundlerKind::Esbuild);
        assert_eq!(
            m.source_path_hint.as_deref(),
            Some("node_modules/lodash/index.js")
        );
        assert!(m.virtual_id.starts_with("esbuild:"));
        assert!(m.body_span.end > m.body_span.start);
        assert_eq!(m.parent_module_id, ModuleId(99));
    }

    #[test]
    fn detect_commonjs_extracts_multiple_entries() {
        let src = r#"
            var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
            var x = __commonJS({
                "a.js": (exports, module) => { module.exports = 1; },
                "b.js": (exports, module) => { module.exports = 2; }
            });
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 2);
        let paths: Vec<_> = modules
            .iter()
            .filter_map(|m| m.source_path_hint.as_deref())
            .collect();
        assert!(paths.contains(&"a.js"));
        assert!(paths.contains(&"b.js"));
    }

    #[test]
    fn detect_commonjs_ignores_calls_with_wrong_callee() {
        let src = r#"
            var x = __notCommonJS({
                "a.js": (exports, module) => { module.exports = 1; }
            });
        "#;
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_commonjs_ignores_calls_with_non_object_arg() {
        let src = r#"var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports); var x = __commonJS([]);"#;
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_commonjs_returns_body_span_not_full_function_span() {
        let src = r#"var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports); var x = __commonJS({ "a": (e, m) => { var y = 1; m.exports = y; } });"#;
        let modules = extract(src);
        let m = &modules[0];
        let body_text = &src[m.body_span.start as usize..m.body_span.end as usize];
        assert!(body_text.starts_with('{'));
        assert!(body_text.ends_with('}'));
        assert!(body_text.contains("var y = 1"));
    }

    #[test]
    fn detect_esm_extracts_zero_arg_arrow_body() {
        let src = r#"
        var __esm=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);
        var x = __esm({
            "lib/foo.js": () => {
                init_lib();
                foo = 1;
            }
        });
    "#;
        let modules = extract_esm(src);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].bundler, BundlerKind::Esbuild);
        assert_eq!(modules[0].source_path_hint.as_deref(), Some("lib/foo.js"));
        assert!(modules[0].virtual_id.starts_with("esbuild:"));
    }

    #[test]
    fn detect_esm_ignores_non_esm_calls() {
        let src = r#"var x = __notEsm({ "a": () => {} });"#;
        assert!(extract_esm(src).is_empty());
    }

    #[test]
    fn var_assignment_module_owns_full_hoisted_statement() {
        // esbuild scope-hoisting puts a module's top-level vars + init handle
        // in the `var` statement, with only the initializer code in the arrow
        // body. The inner module must own the WHOLE statement so the hoisted
        // declarators (`a`, `b`) and the handle (`X`) become definitions,
        // not free variables.
        let src = r#"var st=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);
var a,b,X=st(()=>{a=1;b=2});"#;
        let modules = extract_esm(src);
        assert_eq!(modules.len(), 1, "got: {modules:#?}");
        let m = &modules[0];
        assert_eq!(m.virtual_id, "esbuild:X");
        let owned = &src[m.body_span.start as usize..m.body_span.end as usize];
        assert!(
            owned.starts_with("var "),
            "owned span must be the full statement: {owned}"
        );
        assert!(
            owned.contains("a,b,X="),
            "must include hoisted declarators: {owned}"
        );
        assert!(
            owned.contains("a=1;b=2"),
            "must include the init body: {owned}"
        );
    }

    #[test]
    fn var_assignment_multi_handle_reconstructs_per_handle_synthetic_source() {
        // REAL esbuild scope-hoisting shape, grepped from the Claude index.js
        // (`...WLA}),Uu,oCt=st(()=>{HM...`, `...}),coA,n_A,mNe,ECt=st(()=>{Ra...`):
        // INIT-LESS hoisted declarators (`a`,`b`,`c`) interspersed with multiple
        // `st`(=__esm) lazy-init handles (`X`,`Y`) in ONE `var` statement. Each
        // handle is rebuilt into its own single-handle statement carrying its
        // handle declarator + the hoisted vars its body WRITES, so the handle
        // name becomes a real definition/export (otherwise cross-module
        // `X()`/`Y()` calls dangle — the oCt/ECt-class residual). Write-analysis
        // attributes `a`→X (writes `a=1`) and `b`,`c`→Y (writes `b=2;c=3`).
        let src = r#"var st=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);
var a,X=st(()=>{a=1}),b,c,Y=st(()=>{b=2;c=3});"#;
        let modules = extract_esm(src);
        let synthetic = |vid: &str| {
            modules
                .iter()
                .find(|m| m.virtual_id == vid)
                .and_then(|m| m.synthetic_source.clone())
        };
        assert_eq!(
            synthetic("esbuild:X").as_deref(),
            Some("var a; var X=st(()=>{a=1});"),
            "X reconstructs with its written hoisted var: {modules:#?}"
        );
        assert_eq!(
            synthetic("esbuild:Y").as_deref(),
            Some("var b; var c; var Y=st(()=>{b=2;c=3});"),
            "Y reconstructs with its written hoisted vars: {modules:#?}"
        );
    }

    #[test]
    fn var_assignment_multi_handle_carries_initialized_hoisted_members() {
        // esbuild co-hoists module-local definitions (constants, helper arrows)
        // into the same `var` statement as the init handles. Each such
        // initialized member is carried verbatim into the nearest following
        // handle's reconstructed statement so it stays defined + exportable.
        let src = r#"var st=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);
var iNe="x",a,X=st(()=>{a=1}),HBr=e=>e,Y=st(()=>{});"#;
        let modules = extract_esm(src);
        let synthetic = |vid: &str| {
            modules
                .iter()
                .find(|m| m.virtual_id == vid)
                .and_then(|m| m.synthetic_source.clone())
        };
        assert_eq!(
            synthetic("esbuild:X").as_deref(),
            Some(r#"var iNe="x"; var a; var X=st(()=>{a=1});"#),
            "X carries the preceding initialized member + its written bare var: {modules:#?}"
        );
        assert_eq!(
            synthetic("esbuild:Y").as_deref(),
            Some("var HBr=e=>e; var Y=st(()=>{});"),
            "Y carries its preceding initialized member: {modules:#?}"
        );
    }

    #[test]
    fn detect_commonjs_extracts_minified_var_assignment_modules() {
        // Production esbuild output: helper renamed to `U`, per-module
        // form is `var <name> = U((exports) => { ... })`.
        let src = r#"
            var U=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
            var vG=U((ES0)=>{ES0.foo=1});
            var Gc=U((CS0,m)=>{m.exports={bar:2}});
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 2, "got: {modules:#?}");
        assert!(modules.iter().any(|m| m.virtual_id == "esbuild:vG"));
        assert!(modules.iter().any(|m| m.virtual_id == "esbuild:Gc"));
        // source_path_hint is lost in minification.
        for m in &modules {
            assert_eq!(m.source_path_hint, None);
            assert_eq!(m.bundler, BundlerKind::Esbuild);
        }
    }

    #[test]
    fn detect_esm_extracts_minified_var_assignment_modules() {
        // Production esbuild ESM: helper renamed to `O`, per-module form
        // is `var <name> = O(() => { ... })`.
        let src = r#"
            var O=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);
            var $F1=O(()=>{Ez9=1});
            var Yj=O(()=>{$F1();ZK=2});
        "#;
        let modules = extract_esm(src);
        assert_eq!(modules.len(), 2, "got: {modules:#?}");
        assert!(modules.iter().any(|m| m.virtual_id == "esbuild:$F1"));
        assert!(modules.iter().any(|m| m.virtual_id == "esbuild:Yj"));
    }

    #[test]
    fn detect_commonjs_does_not_emit_for_var_assignment_without_helper() {
        // No helper definition → no aliases → no var-assignment extraction.
        let src = r#"
            var vG=U((ES0)=>{ES0.foo=1});
        "#;
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_esm_does_not_confuse_cjs_alias_with_esm() {
        // Only CJS helper defined; ESM-form var-assignments using O should
        // not match (O has no alias).
        let src = r#"
            var U=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
            var x=O(()=>{a=1});
        "#;
        assert!(extract_esm(src).is_empty());
    }
}
