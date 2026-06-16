use std::collections::{BTreeMap, BTreeSet};

use oxc_ast::{
    AstBuilder, Visit,
    ast::{
        ExportNamedDeclaration, IdentifierReference, ImportDeclaration, ImportDeclarationSpecifier,
        ImportOrExportKind, Program, Statement,
    },
    visit::walk::walk_export_named_declaration,
};
use oxc_span::SPAN;

use crate::module_export_name_text;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SimpleNamedImportSpecifier {
    pub imported: String,
    pub local: String,
}

pub(crate) fn coalesce_imports_in_program<'a>(program: &mut Program<'a>, builder: &AstBuilder<'a>) {
    coalesce_simple_named_imports_in_program(program, builder);
    coalesce_compatible_mixed_imports_in_program(program, builder);
}

fn coalesce_simple_named_imports_in_program<'a>(
    program: &mut Program<'a>,
    builder: &AstBuilder<'a>,
) {
    let mut specifiers_by_source = BTreeMap::<String, BTreeSet<SimpleNamedImportSpecifier>>::new();
    let mut first_index_by_source = BTreeMap::<String, usize>::new();
    let mut duplicate_indices = Vec::<usize>::new();

    for (index, statement) in program.body.iter().enumerate() {
        let Statement::ImportDeclaration(declaration) = statement else {
            continue;
        };
        let Some(specifiers) = simple_named_import_specifiers(declaration) else {
            continue;
        };
        let source = declaration.source.value.as_str().to_string();
        specifiers_by_source
            .entry(source.clone())
            .or_default()
            .extend(specifiers);
        use std::collections::btree_map::Entry;
        match first_index_by_source.entry(source) {
            Entry::Vacant(entry) => {
                entry.insert(index);
            }
            Entry::Occupied(_) => {
                duplicate_indices.push(index);
            }
        }
    }

    if duplicate_indices.is_empty() {
        return;
    }

    for (source, index) in first_index_by_source {
        let Some(specifiers) = specifiers_by_source.get(source.as_str()) else {
            continue;
        };
        program.body[index] =
            simple_named_import_statement(builder, source.as_str(), specifiers.iter());
    }

    for index in duplicate_indices.iter().rev() {
        program.body.remove(*index);
    }
}

fn simple_named_import_specifiers(
    declaration: &ImportDeclaration<'_>,
) -> Option<BTreeSet<SimpleNamedImportSpecifier>> {
    if declaration.import_kind != ImportOrExportKind::Value
        || declaration.phase.is_some()
        || declaration.with_clause.is_some()
    {
        return None;
    }
    let specifiers = declaration.specifiers.as_ref()?;
    if specifiers.is_empty() {
        return None;
    }

    let mut out = BTreeSet::<SimpleNamedImportSpecifier>::new();
    for specifier in specifiers {
        let ImportDeclarationSpecifier::ImportSpecifier(specifier) = specifier else {
            return None;
        };
        if specifier.import_kind != ImportOrExportKind::Value {
            return None;
        }
        out.insert(SimpleNamedImportSpecifier {
            imported: module_export_name_text(&specifier.imported)?,
            local: specifier.local.name.as_str().to_string(),
        });
    }
    Some(out)
}

pub(crate) fn simple_named_import_statement<'a, 'b>(
    builder: &AstBuilder<'a>,
    source: &str,
    specifiers: impl Iterator<Item = &'b SimpleNamedImportSpecifier>,
) -> Statement<'a> {
    let mut generated_specifiers = builder.vec();
    for specifier in specifiers {
        let imported =
            builder.module_export_name_identifier_name(SPAN, specifier.imported.as_str());
        let local = builder.binding_identifier(SPAN, specifier.local.as_str());
        generated_specifiers.push(builder.import_declaration_specifier_import_specifier(
            SPAN,
            imported,
            local,
            ImportOrExportKind::Value,
        ));
    }
    let source = builder.string_literal(SPAN, source, None);
    Statement::ImportDeclaration(builder.alloc_import_declaration(
        SPAN,
        Some(generated_specifiers),
        source,
        None,
        oxc_ast::NONE,
        ImportOrExportKind::Value,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompatibleImportDetails {
    Named(BTreeSet<SimpleNamedImportSpecifier>),
    Default {
        local: String,
    },
    DefaultNamed {
        local: String,
        named: BTreeSet<SimpleNamedImportSpecifier>,
    },
    Namespace {
        local: String,
    },
    DefaultNamespace {
        default_local: String,
        namespace_local: String,
    },
}

#[derive(Debug, Default, Clone)]
struct MixedImportState {
    named_indices: Vec<usize>,
    named_specifiers: BTreeSet<SimpleNamedImportSpecifier>,
    default_target: Option<(usize, String, BTreeSet<SimpleNamedImportSpecifier>)>,
    duplicate_default_indices: Vec<usize>,
    duplicate_default_specifiers: BTreeSet<SimpleNamedImportSpecifier>,
    namespace_target: Option<(usize, String)>,
}

fn coalesce_compatible_mixed_imports_in_program<'a>(
    program: &mut Program<'a>,
    builder: &AstBuilder<'a>,
) {
    let mut states = BTreeMap::<String, MixedImportState>::new();

    for (index, statement) in program.body.iter().enumerate() {
        let Statement::ImportDeclaration(declaration) = statement else {
            continue;
        };
        let Some(details) = compatible_import_details(declaration) else {
            continue;
        };
        let state = states
            .entry(declaration.source.value.as_str().to_string())
            .or_default();
        match details {
            CompatibleImportDetails::Named(specifiers) => {
                state.named_indices.push(index);
                state.named_specifiers.extend(specifiers);
            }
            CompatibleImportDetails::Default { local } => {
                if state.default_target.is_some() {
                    state.duplicate_default_indices.push(index);
                    state
                        .duplicate_default_specifiers
                        .insert(default_import_specifier(local.as_str()));
                } else {
                    state
                        .default_target
                        .get_or_insert_with(|| (index, local, BTreeSet::new()));
                }
            }
            CompatibleImportDetails::DefaultNamed { local, named } => {
                if state.default_target.is_some() {
                    state.duplicate_default_indices.push(index);
                    state
                        .duplicate_default_specifiers
                        .insert(default_import_specifier(local.as_str()));
                    state.named_specifiers.extend(named);
                } else {
                    state.default_target.get_or_insert((index, local, named));
                }
            }
            CompatibleImportDetails::Namespace { local } => {
                state.namespace_target.get_or_insert((index, local));
            }
            CompatibleImportDetails::DefaultNamespace { .. } => {}
        }
    }

    let mut replacements = BTreeMap::<usize, Statement<'a>>::new();
    let mut duplicate_indices = BTreeSet::<usize>::new();
    for (source, state) in states {
        if let Some((default_index, default_local, mut named_specifiers)) =
            state.default_target.clone()
            && (!state.named_indices.is_empty() || !state.duplicate_default_indices.is_empty())
        {
            named_specifiers.extend(state.named_specifiers);
            named_specifiers.extend(state.duplicate_default_specifiers);
            let replacement_index = state
                .named_indices
                .iter()
                .copied()
                .chain(state.duplicate_default_indices.iter().copied())
                .chain([default_index])
                .min()
                .unwrap_or(default_index);
            replacements.insert(
                replacement_index,
                default_named_import_statement(
                    builder,
                    source.as_str(),
                    default_local.as_str(),
                    named_specifiers.iter(),
                ),
            );
            for index in state
                .named_indices
                .iter()
                .copied()
                .chain(state.duplicate_default_indices.iter().copied())
                .chain([default_index])
            {
                if index != replacement_index {
                    duplicate_indices.insert(index);
                }
            }
            continue;
        }

        if state.named_indices.is_empty()
            && let (
                Some((default_index, default_local, default_named_specifiers)),
                Some((namespace_index, namespace_local)),
            ) = (state.default_target, state.namespace_target)
            && default_named_specifiers.is_empty()
        {
            let replacement_index = default_index.min(namespace_index);
            replacements.insert(
                replacement_index,
                default_namespace_import_statement(
                    builder,
                    source.as_str(),
                    default_local.as_str(),
                    namespace_local.as_str(),
                ),
            );
            if default_index != replacement_index {
                duplicate_indices.insert(default_index);
            }
            if namespace_index != replacement_index {
                duplicate_indices.insert(namespace_index);
            }
        }
    }

    if duplicate_indices.is_empty() {
        return;
    }
    let mut merged = builder.vec();
    for (index, statement) in program.body.drain(..).enumerate() {
        if duplicate_indices.contains(&index) {
            continue;
        }
        if let Some(replacement) = replacements.remove(&index) {
            merged.push(replacement);
        } else {
            merged.push(statement);
        }
    }
    program.body = merged;
}

pub(crate) fn compatible_import_details(
    declaration: &ImportDeclaration<'_>,
) -> Option<CompatibleImportDetails> {
    if declaration.import_kind != ImportOrExportKind::Value
        || declaration.phase.is_some()
        || declaration.with_clause.is_some()
    {
        return None;
    }
    let specifiers = declaration.specifiers.as_ref()?;
    if specifiers.is_empty() {
        return None;
    }

    let mut default_local = None::<String>;
    let mut namespace_local = None::<String>;
    let mut named = BTreeSet::<SimpleNamedImportSpecifier>::new();
    for specifier in specifiers {
        match specifier {
            ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
                if default_local.is_some() {
                    return None;
                }
                default_local = Some(specifier.local.name.as_str().to_string());
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                if namespace_local.is_some() {
                    return None;
                }
                namespace_local = Some(specifier.local.name.as_str().to_string());
            }
            ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                if specifier.import_kind != ImportOrExportKind::Value {
                    return None;
                }
                named.insert(SimpleNamedImportSpecifier {
                    imported: module_export_name_text(&specifier.imported)?,
                    local: specifier.local.name.as_str().to_string(),
                });
            }
        }
    }

    match (default_local, namespace_local, named.is_empty()) {
        (Some(local), None, true) => Some(CompatibleImportDetails::Default { local }),
        (Some(local), None, false) => Some(CompatibleImportDetails::DefaultNamed { local, named }),
        (None, Some(local), true) => Some(CompatibleImportDetails::Namespace { local }),
        (Some(default_local), Some(namespace_local), true) => {
            Some(CompatibleImportDetails::DefaultNamespace {
                default_local,
                namespace_local,
            })
        }
        (None, None, false) => Some(CompatibleImportDetails::Named(named)),
        _ => None,
    }
}

pub(crate) fn default_named_import_statement<'a, 'b>(
    builder: &AstBuilder<'a>,
    source: &str,
    default_local: &str,
    named_specifiers: impl Iterator<Item = &'b SimpleNamedImportSpecifier>,
) -> Statement<'a> {
    let mut specifiers = builder.vec1(
        builder.import_declaration_specifier_import_default_specifier(
            SPAN,
            builder.binding_identifier(SPAN, default_local),
        ),
    );
    for specifier in named_specifiers {
        let imported =
            builder.module_export_name_identifier_name(SPAN, specifier.imported.as_str());
        let local = builder.binding_identifier(SPAN, specifier.local.as_str());
        specifiers.push(builder.import_declaration_specifier_import_specifier(
            SPAN,
            imported,
            local,
            ImportOrExportKind::Value,
        ));
    }
    let source = builder.string_literal(SPAN, source, None);
    Statement::ImportDeclaration(builder.alloc_import_declaration(
        SPAN,
        Some(specifiers),
        source,
        None,
        oxc_ast::NONE,
        ImportOrExportKind::Value,
    ))
}

fn default_namespace_import_statement<'a>(
    builder: &AstBuilder<'a>,
    source: &str,
    default_local: &str,
    namespace_local: &str,
) -> Statement<'a> {
    let mut specifiers = builder.vec1(
        builder.import_declaration_specifier_import_default_specifier(
            SPAN,
            builder.binding_identifier(SPAN, default_local),
        ),
    );
    specifiers.push(
        builder.import_declaration_specifier_import_namespace_specifier(
            SPAN,
            builder.binding_identifier(SPAN, namespace_local),
        ),
    );
    let source = builder.string_literal(SPAN, source, None);
    Statement::ImportDeclaration(builder.alloc_import_declaration(
        SPAN,
        Some(specifiers),
        source,
        None,
        oxc_ast::NONE,
        ImportOrExportKind::Value,
    ))
}

pub(crate) fn default_only_import_statement<'a>(
    builder: &AstBuilder<'a>,
    source: &str,
    default_local: &str,
) -> Statement<'a> {
    let specifiers = builder.vec1(
        builder.import_declaration_specifier_import_default_specifier(
            SPAN,
            builder.binding_identifier(SPAN, default_local),
        ),
    );
    let source = builder.string_literal(SPAN, source, None);
    Statement::ImportDeclaration(builder.alloc_import_declaration(
        SPAN,
        Some(specifiers),
        source,
        None,
        oxc_ast::NONE,
        ImportOrExportKind::Value,
    ))
}

fn default_import_specifier(local: &str) -> SimpleNamedImportSpecifier {
    SimpleNamedImportSpecifier {
        imported: "default".to_string(),
        local: local.to_string(),
    }
}

pub(crate) fn prune_unused_import_specifiers_in_program<'a>(
    program: &mut Program<'a>,
    builder: &AstBuilder<'a>,
) {
    let used_bindings = imported_binding_use_set(program);
    if used_bindings.is_empty() {
        return;
    }

    let mut replacements = BTreeMap::<usize, Statement<'a>>::new();
    for (index, statement) in program.body.iter().enumerate() {
        let Statement::ImportDeclaration(declaration) = statement else {
            continue;
        };
        let Some(details) = compatible_import_details(declaration) else {
            continue;
        };
        let source = declaration.source.value.as_str().to_string();
        match details {
            CompatibleImportDetails::Named(named) => {
                let kept = named
                    .iter()
                    .filter(|specifier| used_bindings.contains(specifier.local.as_str()))
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if !kept.is_empty() && kept.len() < named.len() {
                    replacements.insert(
                        index,
                        simple_named_import_statement(builder, source.as_str(), kept.iter()),
                    );
                }
            }
            CompatibleImportDetails::DefaultNamed { local, named } => {
                let keep_default = used_bindings.contains(local.as_str());
                let kept_named = named
                    .iter()
                    .filter(|specifier| used_bindings.contains(specifier.local.as_str()))
                    .cloned()
                    .collect::<BTreeSet<_>>();
                match (keep_default, kept_named.is_empty()) {
                    (true, false) if kept_named.len() < named.len() => {
                        replacements.insert(
                            index,
                            default_named_import_statement(
                                builder,
                                source.as_str(),
                                local.as_str(),
                                kept_named.iter(),
                            ),
                        );
                    }
                    (true, true) => {
                        replacements.insert(
                            index,
                            default_only_import_statement(builder, source.as_str(), local.as_str()),
                        );
                    }
                    (false, false) => {
                        replacements.insert(
                            index,
                            simple_named_import_statement(
                                builder,
                                source.as_str(),
                                kept_named.iter(),
                            ),
                        );
                    }
                    (false, true) => {}
                    (true, false) => {}
                }
            }
            CompatibleImportDetails::Default { .. }
            | CompatibleImportDetails::Namespace { .. }
            | CompatibleImportDetails::DefaultNamespace { .. } => {}
        }
    }

    for (index, replacement) in replacements {
        program.body[index] = replacement;
    }
}

fn imported_binding_use_set(program: &Program<'_>) -> BTreeSet<String> {
    let imported = imported_value_binding_names(program);
    if imported.is_empty() {
        return BTreeSet::new();
    }
    let mut collector = ImportedBindingUseCollector {
        targets: &imported,
        used: BTreeSet::new(),
    };
    collector.visit_program(program);
    collector.used
}

fn imported_value_binding_names(program: &Program<'_>) -> BTreeSet<String> {
    let mut bindings = BTreeSet::<String>::new();
    for statement in &program.body {
        let Statement::ImportDeclaration(declaration) = statement else {
            continue;
        };
        if declaration.import_kind != ImportOrExportKind::Value {
            continue;
        }
        let Some(specifiers) = declaration.specifiers.as_ref() else {
            continue;
        };
        for specifier in specifiers {
            match specifier {
                ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
                    bindings.insert(specifier.local.name.as_str().to_string());
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                    bindings.insert(specifier.local.name.as_str().to_string());
                }
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                    if specifier.import_kind == ImportOrExportKind::Value {
                        bindings.insert(specifier.local.name.as_str().to_string());
                    }
                }
            }
        }
    }
    bindings
}

struct ImportedBindingUseCollector<'a> {
    targets: &'a BTreeSet<String>,
    used: BTreeSet<String>,
}

impl<'a> Visit<'a> for ImportedBindingUseCollector<'_> {
    fn visit_import_declaration(&mut self, _declaration: &ImportDeclaration<'a>) {}

    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if declaration.source.is_some() {
            return;
        }
        for specifier in &declaration.specifiers {
            if let Some(local) = module_export_name_text(&specifier.local)
                && self.targets.contains(local.as_str())
            {
                self.used.insert(local);
            }
        }
        walk_export_named_declaration(self, declaration);
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        let local = identifier.name.as_str();
        if self.targets.contains(local) {
            self.used.insert(local.to_string());
        }
    }
}
