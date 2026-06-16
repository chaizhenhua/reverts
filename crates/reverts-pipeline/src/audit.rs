//! Audit passes for the emit pipeline.
//!
//! Three flavours collected here:
//!   * Pre-plan checks (`audit_required_sources`) — make sure every
//!     non-package module the planner needs has either a real source
//!     body or, failing that, surfaces the gap as a warning so the
//!     pipeline doesn't strand the entire project.
//!   * Plan-synthesis checks (`audit_emit_plan_synthesis`,
//!     `audit_file_synthesis`) — verify the planner's intermediate
//!     representation before emission.
//!   * Post-emit checks (`audit_binding_shape_consistency`,
//!     `audit_namespace_object_member_consistency`,
//!     `audit_emitted_project_parse`) — verify the OXC-rendered TS
//!     output against the plan.

use std::path::Path;

use reverts_emitter::EmittedProject;
use reverts_input::InputBundle;
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
use reverts_js::{
    DeclarationCallability, ParseGoal, classify_top_level_bindings, parse_error_message,
    parse_source,
};
use reverts_model::EnrichedProgram;
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_planner::{EmitPlan, PlannedFile};

pub(crate) fn audit_required_sources(program: &EnrichedProgram) -> AuditReport {
    let mut audit = AuditReport::default();
    for module in program.model().modules() {
        if module.kind == ModuleKind::Package {
            continue;
        }

        if has_module_source(program.model().input(), module.id) {
            continue;
        }

        // A non-package module without a source body means the bundle
        // slice is incomplete for that module. Per ADR 0002 we surface
        // the gap as a warning rather than stranding emission for the
        // whole project; the planner will skip emitting bodies it can't
        // back with source and the audit names the missing binding.
        let definitions = program.model().graph().definitions_for(module.id);
        if definitions.is_empty() {
            audit.push(
                AuditFinding::warning(
                    FindingCode::MissingDefinition,
                    "module has no real source body to emit",
                )
                .with_module(module.id.0.to_string()),
            );
            continue;
        }

        for definition in definitions {
            audit.push(
                AuditFinding::warning(
                    FindingCode::MissingDefinition,
                    "module has symbols but no real source body to emit",
                )
                .with_module(module.id.0.to_string())
                .with_binding(definition.as_str()),
            );
        }
    }
    audit
}

fn has_module_source(input: &InputBundle, module_id: ModuleId) -> bool {
    input.module_source_slice(module_id).is_some()
}

pub(crate) fn audit_emit_plan_synthesis(plan: &EmitPlan) -> AuditReport {
    let mut audit = AuditReport::default();
    for file in &plan.files {
        audit.extend(audit_file_synthesis(file));
    }
    audit
}

fn audit_file_synthesis(file: &PlannedFile) -> AuditReport {
    let mut audit = AuditReport::default();
    let declarations = planned_declarations(file);

    for binding in &file.bindings {
        if !binding.source_backed {
            audit.push(
                AuditFinding::error(
                    FindingCode::SyntheticReferenceWithoutDeclaration,
                    "planned binding has no recovered source declaration",
                )
                .with_module(file.path.clone())
                .with_binding(binding.emitted.as_str()),
            );
        }
    }

    for export in &file.exports {
        if export.source_backed {
            continue;
        }
        if !declarations.contains(&export.binding) {
            audit.push(
                AuditFinding::error(
                    FindingCode::SyntheticReferenceWithoutDeclaration,
                    "planned export references a binding without declaration or import",
                )
                .with_module(file.path.clone())
                .with_binding(export.binding.as_str()),
            );
        }
    }

    audit
}

fn planned_declarations(file: &PlannedFile) -> std::collections::BTreeSet<BindingName> {
    file.imports
        .iter()
        .filter(|import| !import.source_backed)
        .map(|import| import.namespace.clone())
        .chain(file.bindings.iter().map(|binding| binding.emitted.clone()))
        .collect()
}

pub(crate) fn audit_binding_shape_consistency(
    plan: &EmitPlan,
    project: &EmittedProject,
) -> AuditReport {
    let mut audit = AuditReport::default();
    for planned_file in &plan.files {
        let Some(emitted) = project
            .files
            .iter()
            .find(|file| file.path == planned_file.path)
        else {
            continue;
        };
        let classifications = classify_top_level_bindings(
            emitted.source.as_str(),
            Some(Path::new(emitted.path.as_str())),
            ParseGoal::TypeScript,
        );
        for binding in &planned_file.bindings {
            if !binding.source_backed || binding.shape != BindingShape::Callable {
                continue;
            }
            if classifications.get(binding.emitted.as_str())
                == Some(&DeclarationCallability::NotCallable)
            {
                audit.push(
                    AuditFinding::error(
                        FindingCode::CallableEmittedAsNonCallable,
                        "source-backed binding declared as a non-callable value is called like a function — likely a runtime error in the input",
                    )
                    .with_module(planned_file.path.clone())
                    .with_binding(binding.emitted.as_str()),
                );
            }
        }
    }
    audit
}

/// Paper #7 downstream consumer: for every planned `NamespaceObject`
/// binding, every property name the planner recorded must still appear
/// in the emitted source. Catches refactors that silently strip a member
/// from a namespace surface. Uses a simple whole-word identifier match
/// on the emitted text — false positives would only happen if a member
/// name is shadowed by a literal string of the same name, which would
/// itself be a real concern worth flagging.
pub(crate) fn audit_namespace_object_member_consistency(
    plan: &EmitPlan,
    project: &EmittedProject,
) -> AuditReport {
    let mut audit = AuditReport::default();
    for planned_file in &plan.files {
        let Some(emitted) = project
            .files
            .iter()
            .find(|file| file.path == planned_file.path)
        else {
            continue;
        };
        for binding in &planned_file.bindings {
            if binding.shape != BindingShape::NamespaceObject || binding.known_members.is_empty() {
                continue;
            }
            let namespace = binding.emitted.as_str();
            let missing: Vec<&str> = binding
                .known_members
                .iter()
                .map(|member| member.as_str())
                .filter(|member| {
                    !namespace_member_access_present(emitted.source.as_str(), namespace, member)
                        && !named_import_specifier_present(emitted.source.as_str(), member)
                        && !object_destructure_specifier_present(
                            emitted.source.as_str(),
                            namespace,
                            member,
                        )
                })
                .collect();
            if missing.is_empty() {
                continue;
            }
            audit.push(
                AuditFinding::error(
                    FindingCode::NamespaceMemberStripped,
                    format!(
                        "namespace binding lost member access for: {}",
                        missing.join(", "),
                    ),
                )
                .with_module(planned_file.path.clone())
                .with_binding(binding.emitted.as_str()),
            );
        }
    }
    audit
}

/// True when `source` contains any of:
///   - `<namespace>.<member>`  (dot access; member must be a valid identifier)
///   - `<namespace>["<member>"]`  (double-quoted computed access)
///   - `<namespace>['<member>']`  (single-quoted computed access)
///
/// Members that are not valid identifiers can only appear through quoted
/// forms; bundlers like esbuild also routinely quote reserved or
/// non-identifier names. Matching any of the three forms keeps the audit
/// from false-firing on those cases.
fn namespace_member_access_present(source: &str, namespace: &str, member: &str) -> bool {
    if is_member_name_identifier(member)
        && contains_identifier(source, &format!("{namespace}.{member}"))
    {
        return true;
    }
    let double_quoted = format!("{namespace}[\"{member}\"]");
    if contains_identifier(source, &double_quoted) {
        return true;
    }
    let single_quoted = format!("{namespace}['{member}']");
    contains_identifier(source, &single_quoted)
}

fn named_import_specifier_present(source: &str, member: &str) -> bool {
    for import_tail in source.split("import ").skip(1) {
        let Some((import_clause, _)) = import_tail.split_once(" from ") else {
            continue;
        };
        let Some(start) = import_clause.find('{') else {
            continue;
        };
        let Some(end) = import_clause[start + 1..].find('}') else {
            continue;
        };
        let named_specifiers = &import_clause[start + 1..start + 1 + end];
        for specifier in named_specifiers.split(',') {
            let specifier = specifier.trim();
            if specifier == member
                || specifier
                    .strip_prefix(member)
                    .is_some_and(|rest| rest.trim_start().starts_with("as "))
                || specifier
                    .rsplit_once(" as ")
                    .is_some_and(|(_, local)| local.trim() == member)
            {
                return true;
            }
        }
    }
    false
}

fn object_destructure_specifier_present(source: &str, namespace: &str, member: &str) -> bool {
    let mut cursor = 0usize;
    while let Some(offset) = source[cursor..].find('=') {
        let equals = cursor + offset;
        cursor = equals + 1;

        let after_equals = source[equals + 1..].trim_start();
        if !starts_with_identifier(after_equals, namespace) {
            continue;
        }

        let before_equals = source[..equals].trim_end();
        let Some(close_brace) = before_equals.rfind('}') else {
            continue;
        };
        if close_brace + 1 != before_equals.len() {
            continue;
        }
        let Some(open_brace) = matching_open_brace_before(before_equals, close_brace) else {
            continue;
        };
        let specifiers = &before_equals[open_brace + 1..close_brace];
        if destructure_specifier_mentions_member(specifiers, member) {
            return true;
        }
    }
    false
}

fn starts_with_identifier(source: &str, identifier: &str) -> bool {
    let Some(rest) = source.strip_prefix(identifier) else {
        return false;
    };
    rest.as_bytes()
        .first()
        .is_none_or(|byte| !is_identifier_part(*byte))
}

fn matching_open_brace_before(source: &str, close_brace: usize) -> Option<usize> {
    let mut depth = 0usize;
    for index in (0..=close_brace).rev() {
        match source.as_bytes()[index] {
            b'}' => depth += 1,
            b'{' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn destructure_specifier_mentions_member(specifiers: &str, member: &str) -> bool {
    specifiers.split(',').any(|specifier| {
        let specifier = specifier.trim();
        specifier == member
            || specifier
                .strip_prefix(member)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
    })
}

fn is_member_name_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return false;
    }
    bytes.all(is_identifier_part)
}

fn contains_identifier(source: &str, identifier: &str) -> bool {
    let identifier_bytes = identifier.as_bytes();
    if identifier_bytes.is_empty() {
        return false;
    }
    let source_bytes = source.as_bytes();
    let mut cursor = 0;
    while let Some(offset) = source[cursor..].find(identifier) {
        let start = cursor + offset;
        let end = start + identifier_bytes.len();
        let before_ok = start == 0 || !is_identifier_part(source_bytes[start - 1]);
        let after_ok = end >= source_bytes.len() || !is_identifier_part(source_bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        cursor = start + 1;
    }
    false
}

const fn is_identifier_part(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

pub(crate) fn audit_emitted_project_parse(project: &EmittedProject) -> AuditReport {
    let mut audit = AuditReport::default();
    for file in &project.files {
        if let Err(error) = parse_source(
            file.source.as_str(),
            Some(Path::new(file.path.as_str())),
            ParseGoal::TypeScript,
        ) {
            audit.push(
                AuditFinding::error(
                    FindingCode::UnparseableOutput,
                    parse_error_message(&error, "output could not be parsed"),
                )
                .with_module(file.path.clone()),
            );
        }
    }
    audit
}
