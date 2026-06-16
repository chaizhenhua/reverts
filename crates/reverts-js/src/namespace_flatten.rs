use std::collections::{BTreeMap, BTreeSet};

use oxc_ast::{
    AstBuilder, Visit, VisitMut,
    ast::{
        BindingIdentifier, ExportNamedDeclaration, Expression, IdentifierReference,
        ImportDeclaration, ImportDeclarationSpecifier, ImportOrExportKind, Program, Statement,
        StaticMemberExpression,
    },
    visit::walk::walk_export_named_declaration,
};
use oxc_span::SPAN;

use crate::identifier::sanitize_identifier;
use crate::import_coalesce::{
    CompatibleImportDetails, SimpleNamedImportSpecifier, compatible_import_details,
    default_named_import_statement, default_only_import_statement, simple_named_import_statement,
};
use crate::module_export_name_text;

#[derive(Debug, Default, Clone)]
struct NamespaceFlattenSourceState {
    namespace_imports: Vec<NamespaceFlattenImport>,
    existing_named_aliases: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct NamespaceFlattenImport {
    index: usize,
    namespace_local: String,
    default_local: Option<String>,
}

pub(crate) fn flatten_node_builtin_namespace_imports_in_program<'a>(
    program: &mut Program<'a>,
    builder: &AstBuilder<'a>,
) {
    let mut states = BTreeMap::<String, NamespaceFlattenSourceState>::new();
    for (index, statement) in program.body.iter().enumerate() {
        let Statement::ImportDeclaration(declaration) = statement else {
            continue;
        };
        if !import_declaration_can_be_merged(declaration)
            || !is_node_builtin_namespace_flatten_source(declaration.source.value.as_str())
        {
            continue;
        }
        let source = declaration.source.value.as_str().to_string();
        let state = states.entry(source).or_default();
        match compatible_import_details(declaration) {
            Some(CompatibleImportDetails::Namespace { local }) => {
                state.namespace_imports.push(NamespaceFlattenImport {
                    index,
                    namespace_local: local,
                    default_local: None,
                });
            }
            Some(CompatibleImportDetails::DefaultNamespace {
                default_local,
                namespace_local,
            }) => {
                state.namespace_imports.push(NamespaceFlattenImport {
                    index,
                    namespace_local,
                    default_local: Some(default_local),
                });
            }
            _ => {}
        }
        for specifier in named_import_aliases(declaration) {
            state
                .existing_named_aliases
                .entry(specifier.imported.clone())
                .and_modify(|current| {
                    if specifier.local.len() < current.len() {
                        *current = specifier.local.clone();
                    }
                })
                .or_insert(specifier.local);
        }
    }
    states.retain(|_, state| !state.namespace_imports.is_empty());
    if states.is_empty() {
        return;
    }

    let namespace_locals = states
        .values()
        .flat_map(|state| {
            state
                .namespace_imports
                .iter()
                .map(|import| import.namespace_local.clone())
        })
        .collect::<BTreeSet<_>>();
    let mut analysis = NamespaceImportUseAnalysis {
        namespace_locals: &namespace_locals,
        properties_by_local: BTreeMap::new(),
        bad_locals: BTreeSet::new(),
    };
    analysis.visit_program(program);

    let mut binding_names = BindingNameCollector::default();
    binding_names.visit_program(program);

    let mut member_replacements = BTreeMap::<(String, String), String>::new();
    let mut generated_specifiers_by_source =
        BTreeMap::<String, BTreeSet<SimpleNamedImportSpecifier>>::new();
    let mut default_import_replacements = BTreeMap::<usize, (String, String)>::new();
    let mut removable_namespace_indices = BTreeSet::<usize>::new();
    for (source, state) in &states {
        for import in &state.namespace_imports {
            let namespace_local = import.namespace_local.as_str();
            if analysis.bad_locals.contains(namespace_local) {
                continue;
            }
            let Some(properties) = analysis.properties_by_local.get(namespace_local) else {
                continue;
            };
            if properties.is_empty() || properties.contains("default") {
                continue;
            }

            let mut local_replacements = Vec::<(String, String)>::new();
            let mut generated_specifiers = Vec::<SimpleNamedImportSpecifier>::new();
            for property in properties {
                if let Some(existing) = state.existing_named_aliases.get(property) {
                    local_replacements.push((property.clone(), existing.clone()));
                    continue;
                }
                let alias =
                    unique_namespace_member_alias(namespace_local, property, &mut binding_names);
                generated_specifiers.push(SimpleNamedImportSpecifier {
                    imported: property.clone(),
                    local: alias.clone(),
                });
                local_replacements.push((property.clone(), alias));
            }

            for specifier in generated_specifiers {
                generated_specifiers_by_source
                    .entry(source.clone())
                    .or_default()
                    .insert(specifier);
            }
            for (property, alias) in local_replacements {
                member_replacements.insert((namespace_local.to_string(), property), alias);
            }
            if let Some(default_local) = &import.default_local {
                default_import_replacements
                    .insert(import.index, (source.clone(), default_local.clone()));
            }
            removable_namespace_indices.insert(import.index);
        }
    }
    if removable_namespace_indices.is_empty() {
        return;
    }

    let mut rewriter = NamespaceMemberRewriter {
        replacements: &member_replacements,
        builder,
    };
    rewriter.visit_program(program);

    let mut replacement_by_index = BTreeMap::<usize, Statement<'a>>::new();
    for (source, specifiers) in generated_specifiers_by_source {
        if specifiers.is_empty() {
            continue;
        }
        let Some(first_index) = states.get(source.as_str()).and_then(|state| {
            state
                .namespace_imports
                .iter()
                .map(|import| import.index)
                .filter(|index| removable_namespace_indices.contains(index))
                .min()
        }) else {
            continue;
        };
        if let Some((_source, default_local)) = default_import_replacements.remove(&first_index) {
            replacement_by_index.insert(
                first_index,
                default_named_import_statement(
                    builder,
                    source.as_str(),
                    default_local.as_str(),
                    specifiers.iter(),
                ),
            );
        } else {
            replacement_by_index.insert(
                first_index,
                simple_named_import_statement(builder, source.as_str(), specifiers.iter()),
            );
        }
    }
    for (index, (source, default_local)) in default_import_replacements {
        if removable_namespace_indices.contains(&index) {
            replacement_by_index.insert(
                index,
                default_only_import_statement(builder, source.as_str(), default_local.as_str()),
            );
        }
    }

    let mut merged = builder.vec();
    for (index, statement) in program.body.drain(..).enumerate() {
        if let Some(replacement) = replacement_by_index.remove(&index) {
            merged.push(replacement);
            continue;
        }
        if removable_namespace_indices.contains(&index) {
            continue;
        }
        merged.push(statement);
    }
    program.body = merged;
}

fn import_declaration_can_be_merged(declaration: &ImportDeclaration<'_>) -> bool {
    declaration.import_kind == ImportOrExportKind::Value
        && declaration.phase.is_none()
        && declaration.with_clause.is_none()
}

fn named_import_aliases(
    declaration: &ImportDeclaration<'_>,
) -> BTreeSet<SimpleNamedImportSpecifier> {
    let Some(specifiers) = declaration.specifiers.as_ref() else {
        return BTreeSet::new();
    };
    specifiers
        .iter()
        .filter_map(|specifier| {
            let ImportDeclarationSpecifier::ImportSpecifier(specifier) = specifier else {
                return None;
            };
            if specifier.import_kind != ImportOrExportKind::Value {
                return None;
            }
            Some(SimpleNamedImportSpecifier {
                imported: module_export_name_text(&specifier.imported)?,
                local: specifier.local.name.as_str().to_string(),
            })
        })
        .collect()
}

#[derive(Debug)]
struct NamespaceImportUseAnalysis<'a> {
    namespace_locals: &'a BTreeSet<String>,
    properties_by_local: BTreeMap<String, BTreeSet<String>>,
    bad_locals: BTreeSet<String>,
}

impl<'a> Visit<'a> for NamespaceImportUseAnalysis<'_> {
    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if declaration.source.is_none() {
            for specifier in &declaration.specifiers {
                if let Some(local) = module_export_name_text(&specifier.local)
                    && self.namespace_locals.contains(local.as_str())
                {
                    self.bad_locals.insert(local);
                }
            }
        }
        walk_export_named_declaration(self, declaration);
    }

    fn visit_static_member_expression(&mut self, expression: &StaticMemberExpression<'a>) {
        if let Expression::Identifier(object) = &expression.object {
            let local = object.name.as_str();
            if self.namespace_locals.contains(local) {
                self.properties_by_local
                    .entry(local.to_string())
                    .or_default()
                    .insert(expression.property.name.as_str().to_string());
                return;
            }
        }
        oxc_ast::visit::walk::walk_static_member_expression(self, expression);
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        let local = identifier.name.as_str();
        if self.namespace_locals.contains(local) {
            self.bad_locals.insert(local.to_string());
        }
    }
}

#[derive(Debug, Default)]
struct BindingNameCollector {
    names: BTreeSet<String>,
}

impl<'a> Visit<'a> for BindingNameCollector {
    fn visit_binding_identifier(&mut self, identifier: &BindingIdentifier<'a>) {
        self.names.insert(identifier.name.as_str().to_string());
    }
}

fn unique_namespace_member_alias(
    namespace_local: &str,
    property: &str,
    binding_names: &mut BindingNameCollector,
) -> String {
    let base = sanitize_identifier(format!("__reverts_{namespace_local}_{property}").as_str());
    let mut candidate = base.clone();
    let mut suffix = 2usize;
    while binding_names.names.contains(candidate.as_str()) {
        candidate = format!("{base}_{suffix}");
        suffix += 1;
    }
    binding_names.names.insert(candidate.clone());
    candidate
}

struct NamespaceMemberRewriter<'b, 'a> {
    replacements: &'b BTreeMap<(String, String), String>,
    builder: &'b AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for NamespaceMemberRewriter<'_, 'a> {
    fn visit_expression(&mut self, expression: &mut Expression<'a>) {
        if let Expression::StaticMemberExpression(member) = expression
            && let Expression::Identifier(object) = &member.object
        {
            let key = (
                object.name.as_str().to_string(),
                member.property.name.as_str().to_string(),
            );
            if let Some(replacement) = self.replacements.get(&key) {
                *expression = self
                    .builder
                    .expression_identifier_reference(SPAN, replacement.as_str());
                return;
            }
        }
        oxc_ast::visit::walk_mut::walk_expression(self, expression);
    }
}

fn is_node_builtin_namespace_flatten_source(source: &str) -> bool {
    let source = source.strip_prefix("node:").unwrap_or(source);
    matches!(
        source,
        "assert"
            | "async_hooks"
            | "buffer"
            | "child_process"
            | "crypto"
            | "events"
            | "fs"
            | "fs/promises"
            | "http"
            | "https"
            | "module"
            | "net"
            | "os"
            | "path"
            | "perf_hooks"
            | "process"
            | "querystring"
            | "readline"
            | "stream"
            | "timers"
            | "tty"
            | "url"
            | "util"
            | "worker_threads"
            | "zlib"
    )
}
