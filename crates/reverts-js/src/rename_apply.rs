use std::collections::{BTreeMap, BTreeSet};

use oxc_allocator::Allocator;
use oxc_ast::{
    AstBuilder, Visit, VisitMut,
    ast::{
        BindingIdentifier, BindingPatternKind, BindingProperty, ExportNamedDeclaration,
        IdentifierReference, ImportDeclaration, ImportDeclarationSpecifier, ModuleExportName,
        ObjectProperty, Program, TemplateElement,
    },
};
use oxc_parser::Parser;
use oxc_semantic::{SemanticBuilder, SymbolTable};
use oxc_syntax::{
    reference::ReferenceId,
    symbol::{SymbolFlags, SymbolId},
};

use crate::ReadabilityReport;
use crate::errors::{JsError, ParseError, ParseGoal, Result};
use crate::identifier::sanitize_identifier;
use crate::module_export_name_text;
use crate::parse::{parse_options_for, source_type_candidates};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ReadabilityRenameSource {
    UsagePattern,
    ObjectProperty,
    PackageNamespace,
    ImportExportPublic,
    CommonJsExport,
    ExplicitSemantic,
}

impl ReadabilityRenameSource {
    fn confidence(self) -> u8 {
        match self {
            Self::ExplicitSemantic => 100,
            Self::ImportExportPublic | Self::CommonJsExport => 90,
            Self::PackageNamespace => 80,
            Self::ObjectProperty => 50,
            Self::UsagePattern => 40,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitSemantic => "explicit_semantic",
            Self::ImportExportPublic => "import_export_public",
            Self::CommonJsExport => "commonjs_export",
            Self::PackageNamespace => "package_namespace",
            Self::ObjectProperty => "object_property",
            Self::UsagePattern => "usage_pattern",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ReadabilityRenameHint {
    original: String,
    renamed: String,
    source: ReadabilityRenameSource,
}

impl ReadabilityRenameHint {
    pub(crate) fn new(original: &str, renamed: &str, source: ReadabilityRenameSource) -> Self {
        Self {
            original: original.trim().to_string(),
            renamed: renamed.trim().to_string(),
            source,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedReadabilityRename {
    original: String,
    renamed: String,
    source: ReadabilityRenameSource,
}

pub(crate) fn resolve_readability_rename_hints(
    hints: Vec<ReadabilityRenameHint>,
    report: &mut ReadabilityReport,
) -> Vec<ResolvedReadabilityRename> {
    let mut hints_by_original = BTreeMap::<String, Vec<ReadabilityRenameHint>>::new();
    for hint in hints {
        if hint.original.is_empty() || hint.renamed.is_empty() || hint.original == hint.renamed {
            continue;
        }
        if sanitize_identifier(hint.renamed.as_str()) != hint.renamed {
            report.push(format!(
                "skipped rename {} -> {}, source={}, reason=invalid_target",
                hint.original,
                hint.renamed,
                hint.source.as_str()
            ));
            continue;
        }
        hints_by_original
            .entry(hint.original.clone())
            .or_default()
            .push(hint);
    }

    let mut resolved = Vec::new();
    for (original, hints) in hints_by_original {
        let max_confidence = hints
            .iter()
            .map(|hint| hint.source.confidence())
            .max()
            .unwrap_or(0);
        let top_hints = hints
            .iter()
            .filter(|hint| hint.source.confidence() == max_confidence)
            .collect::<Vec<_>>();
        let top_names = top_hints
            .iter()
            .map(|hint| hint.renamed.as_str())
            .collect::<BTreeSet<_>>();
        if top_names.len() != 1 {
            let candidates = top_hints
                .iter()
                .map(|hint| format!("{}:{}", hint.source.as_str(), hint.renamed))
                .collect::<Vec<_>>()
                .join(", ");
            report.push(format!(
                "skipped rename {}, reason=conflicting_hints, candidates={candidates}",
                original
            ));
            continue;
        }
        let chosen = top_hints[0];
        resolved.push(ResolvedReadabilityRename {
            original,
            renamed: chosen.renamed.clone(),
            source: chosen.source,
        });
    }
    resolved
}

pub(crate) fn apply_readability_renames<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    readability_renames: &[ResolvedReadabilityRename],
    report: &mut ReadabilityReport,
) {
    let requested = readability_renames
        .iter()
        .map(|rename| (rename.original.clone(), rename.clone()))
        .collect::<BTreeMap<_, _>>();
    if requested.is_empty() {
        return;
    }

    let (symbol_renames, reference_renames) = {
        let semantic = SemanticBuilder::new().build(program).semantic;
        let symbols = semantic.symbols();
        let root_scope_id = semantic.scopes().root_scope_id();
        let unresolved_root_names = semantic
            .scopes()
            .root_unresolved_references()
            .keys()
            .map(|name| name.as_str().to_string())
            .collect::<BTreeSet<_>>();
        let root_symbols = symbols
            .symbol_ids()
            .filter(|symbol_id| symbols.get_scope_id(*symbol_id) == root_scope_id)
            .collect::<Vec<_>>();

        let mut symbol_renames = BTreeMap::<SymbolId, String>::new();
        for (original, rename) in &requested {
            let renamed = rename.renamed.as_str();
            // If the desired name is already used as a free global reference,
            // introducing a module-scope binding with that name would change
            // resolution for nested reads. Leave the original name intact.
            if unresolved_root_names.contains(renamed) {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=would_capture_global",
                    rename.source.as_str()
                ));
                continue;
            }
            let targets = root_symbols
                .iter()
                .copied()
                .filter(|symbol_id| symbols.get_name(*symbol_id) == original)
                .collect::<Vec<_>>();
            if targets.len() != 1 {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=missing_or_ambiguous_original",
                    rename.source.as_str()
                ));
                continue;
            }
            let target = targets[0];
            let collides = root_symbols
                .iter()
                .copied()
                .any(|symbol_id| symbol_id != target && symbols.get_name(symbol_id) == renamed);
            if collides {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=name_collision",
                    rename.source.as_str()
                ));
                continue;
            }
            if symbol_renames.values().any(|value| value == renamed) {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=duplicate_target",
                    rename.source.as_str()
                ));
                continue;
            }
            symbol_renames.insert(target, renamed.to_string());
            report.push(format!(
                "renamed {original} -> {renamed}, source={}",
                rename.source.as_str()
            ));
        }

        let mut reference_renames = BTreeMap::<ReferenceId, String>::new();
        for (symbol_id, renamed) in &symbol_renames {
            for reference_id in symbols.get_resolved_reference_ids(*symbol_id) {
                reference_renames.insert(*reference_id, renamed.clone());
            }
        }

        (symbol_renames, reference_renames)
    };

    if symbol_renames.is_empty() && reference_renames.is_empty() {
        return;
    }

    let mut renamer = ReadabilityRenamer {
        builder: AstBuilder::new(allocator),
        symbol_renames,
        reference_renames,
    };
    renamer.visit_program(program);
}

pub(crate) fn apply_emit_safety_renames<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    report: &mut ReadabilityReport,
) {
    let (symbol_renames, reference_renames) = {
        let semantic = SemanticBuilder::new().build(program).semantic;
        let symbols = semantic.symbols();
        let mut used_names = symbols
            .symbol_ids()
            .map(|symbol_id| symbols.get_name(symbol_id).to_string())
            .collect::<BTreeSet<_>>();
        used_names.extend(
            semantic
                .scopes()
                .root_unresolved_references()
                .keys()
                .map(|name| name.as_str().to_string()),
        );

        let mut symbol_renames = BTreeMap::<SymbolId, String>::new();
        let mut reference_renames = BTreeMap::<ReferenceId, String>::new();
        for symbol_id in symbols.symbol_ids() {
            let original = symbols.get_name(symbol_id);
            let sanitized = sanitize_identifier(original);
            if sanitized == original {
                continue;
            }
            let renamed = unique_safe_identifier(&sanitized, &mut used_names);
            symbol_renames.insert(symbol_id, renamed.clone());
            for reference_id in symbols.get_resolved_reference_ids(symbol_id) {
                reference_renames.insert(*reference_id, renamed.clone());
            }
            report.push(format!(
                "renamed {original} -> {renamed}, source=emit_safety"
            ));
        }

        (symbol_renames, reference_renames)
    };

    if symbol_renames.is_empty() && reference_renames.is_empty() {
        return;
    }

    let mut renamer = ReadabilityRenamer {
        builder: AstBuilder::new(allocator),
        symbol_renames,
        reference_renames,
    };
    renamer.visit_program(program);
}

pub(crate) fn apply_generated_semantic_binding_renames<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    report: &mut ReadabilityReport,
) {
    let (symbol_renames, reference_renames) = generated_semantic_binding_renames(program);
    if !symbol_renames.is_empty() {
        report.push(format!(
            "generated semantic binding names for {} minified binding(s)",
            symbol_renames.len()
        ));
    }

    if symbol_renames.is_empty() && reference_renames.is_empty() {
        return;
    }

    let mut renamer = ReadabilityRenamer {
        builder: AstBuilder::new(allocator),
        symbol_renames,
        reference_renames,
    };
    renamer.visit_program(program);
}

pub fn apply_generated_semantic_binding_renames_preserving_source(
    source: &str,
    path_hint: Option<&std::path::Path>,
    goal: ParseGoal,
) -> Result<Option<String>> {
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let template_raws_before = collect_template_raws(&parsed.program);
        let (symbol_renames, reference_renames) =
            generated_semantic_binding_renames_for_source_preservation(&parsed.program);
        if symbol_renames.is_empty() && reference_renames.is_empty() {
            return Ok(None);
        }

        let mut collector = SourceRenameSpanCollector {
            symbol_renames: &symbol_renames,
            reference_renames: &reference_renames,
            replacements: Vec::new(),
        };
        collector.visit_program(&parsed.program);
        let renamed = apply_source_replacements(source, collector.replacements);

        if !source_preserving_rename_is_valid(
            renamed.as_str(),
            source_type,
            template_raws_before.as_slice(),
        ) {
            return Ok(None);
        }
        return Ok(Some(renamed));
    }

    Err(JsError::ParseFailed(errors))
}

fn generated_semantic_binding_renames(
    program: &Program<'_>,
) -> (BTreeMap<SymbolId, String>, BTreeMap<ReferenceId, String>) {
    let semantic = SemanticBuilder::new().build(program).semantic;
    let symbols = semantic.symbols();
    let exported_symbols = exported_specifier_symbols(program, symbols);
    let mut used_names = symbols
        .symbol_ids()
        .map(|symbol_id| symbols.get_name(symbol_id).to_string())
        .collect::<BTreeSet<_>>();
    used_names.extend(
        semantic
            .scopes()
            .root_unresolved_references()
            .keys()
            .map(|name| name.as_str().to_string()),
    );

    let mut symbol_renames = BTreeMap::<SymbolId, String>::new();
    let mut reference_renames = BTreeMap::<ReferenceId, String>::new();
    let mut next_suffix_by_base = BTreeMap::<String, usize>::new();
    for symbol_id in symbols.symbol_ids() {
        let original = symbols.get_name(symbol_id);
        let flags = symbols.get_flags(symbol_id);
        if !should_assign_generated_semantic_name(
            original,
            flags,
            exported_symbols.contains(&symbol_id),
        ) {
            continue;
        }
        let renamed = unique_safe_identifier_with_counters(
            semantic_binding_name_base(flags),
            &mut used_names,
            &mut next_suffix_by_base,
        );
        symbol_renames.insert(symbol_id, renamed.clone());
        for reference_id in symbols.get_resolved_reference_ids(symbol_id) {
            reference_renames.insert(*reference_id, renamed.clone());
        }
    }
    (symbol_renames, reference_renames)
}

fn generated_semantic_binding_renames_for_source_preservation(
    program: &Program<'_>,
) -> (BTreeMap<SymbolId, String>, BTreeMap<ReferenceId, String>) {
    let semantic = SemanticBuilder::new().build(program).semantic;
    let symbols = semantic.symbols();
    let exported_symbols = exported_specifier_symbols(program, symbols);
    let unsafe_symbols = source_preserving_unsafe_symbols(program, symbols);
    let safe_import_symbols = source_preserving_safe_import_symbols(program);
    let mut used_names = symbols
        .symbol_ids()
        .map(|symbol_id| symbols.get_name(symbol_id).to_string())
        .collect::<BTreeSet<_>>();
    used_names.extend(
        semantic
            .scopes()
            .root_unresolved_references()
            .keys()
            .map(|name| name.as_str().to_string()),
    );

    let mut symbol_renames = BTreeMap::<SymbolId, String>::new();
    let mut reference_renames = BTreeMap::<ReferenceId, String>::new();
    let mut next_suffix_by_base = BTreeMap::<String, usize>::new();
    for symbol_id in symbols.symbol_ids() {
        let original = symbols.get_name(symbol_id);
        let flags = symbols.get_flags(symbol_id);
        if unsafe_symbols.contains(&symbol_id)
            || !should_assign_generated_semantic_name_for_source_preservation(
                original,
                flags,
                exported_symbols.contains(&symbol_id),
                safe_import_symbols.contains(&symbol_id),
            )
        {
            continue;
        }
        let renamed = unique_safe_identifier_with_counters(
            semantic_binding_name_base(flags),
            &mut used_names,
            &mut next_suffix_by_base,
        );
        symbol_renames.insert(symbol_id, renamed.clone());
        for reference_id in symbols.get_resolved_reference_ids(symbol_id) {
            reference_renames.insert(*reference_id, renamed.clone());
        }
    }
    (symbol_renames, reference_renames)
}

fn exported_specifier_symbols(program: &Program<'_>, symbols: &SymbolTable) -> BTreeSet<SymbolId> {
    let mut collector = ExportedSpecifierSymbolCollector {
        symbols,
        exported_symbols: BTreeSet::new(),
    };
    collector.visit_program(program);
    collector.exported_symbols
}

struct ExportedSpecifierSymbolCollector<'b> {
    symbols: &'b SymbolTable,
    exported_symbols: BTreeSet<SymbolId>,
}

impl<'a> Visit<'a> for ExportedSpecifierSymbolCollector<'_> {
    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if declaration.source.is_none() {
            for specifier in &declaration.specifiers {
                let ModuleExportName::IdentifierReference(local) = &specifier.local else {
                    continue;
                };
                let Some(reference_id) = local.reference_id.get() else {
                    continue;
                };
                if let Some(symbol_id) = self.symbols.get_reference(reference_id).symbol_id() {
                    self.exported_symbols.insert(symbol_id);
                }
            }
        }
        oxc_ast::visit::walk::walk_export_named_declaration(self, declaration);
    }
}

fn source_preserving_safe_import_symbols(program: &Program<'_>) -> BTreeSet<SymbolId> {
    let mut collector = SourcePreservingSafeImportCollector {
        safe_import_symbols: BTreeSet::new(),
    };
    collector.visit_program(program);
    collector.safe_import_symbols
}

struct SourcePreservingSafeImportCollector {
    safe_import_symbols: BTreeSet<SymbolId>,
}

impl<'a> Visit<'a> for SourcePreservingSafeImportCollector {
    fn visit_import_declaration(&mut self, declaration: &ImportDeclaration<'a>) {
        let Some(specifiers) = &declaration.specifiers else {
            return;
        };
        for specifier in specifiers {
            match specifier {
                ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
                    if let Some(symbol_id) = specifier.local.symbol_id.get() {
                        self.safe_import_symbols.insert(symbol_id);
                    }
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                    if let Some(symbol_id) = specifier.local.symbol_id.get() {
                        self.safe_import_symbols.insert(symbol_id);
                    }
                }
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                    if module_export_name_text(&specifier.imported)
                        .is_some_and(|imported| imported != specifier.local.name.as_str())
                        && let Some(symbol_id) = specifier.local.symbol_id.get()
                    {
                        self.safe_import_symbols.insert(symbol_id);
                    }
                }
            }
        }
        oxc_ast::visit::walk::walk_import_declaration(self, declaration);
    }
}

fn source_preserving_unsafe_symbols(
    program: &Program<'_>,
    symbols: &SymbolTable,
) -> BTreeSet<SymbolId> {
    let mut collector = SourcePreservingUnsafeSymbolCollector {
        symbols,
        unsafe_symbols: BTreeSet::new(),
    };
    collector.visit_program(program);
    collector.unsafe_symbols
}

struct SourcePreservingUnsafeSymbolCollector<'b> {
    symbols: &'b SymbolTable,
    unsafe_symbols: BTreeSet<SymbolId>,
}

impl<'a> Visit<'a> for SourcePreservingUnsafeSymbolCollector<'_> {
    fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
        if property.shorthand
            && let oxc_ast::ast::Expression::Identifier(identifier) = &property.value
            && let Some(reference_id) = identifier.reference_id.get()
            && let Some(symbol_id) = self.symbols.get_reference(reference_id).symbol_id()
        {
            self.unsafe_symbols.insert(symbol_id);
        }
        oxc_ast::visit::walk::walk_object_property(self, property);
    }

    fn visit_binding_property(&mut self, property: &BindingProperty<'a>) {
        if property.shorthand
            && let BindingPatternKind::BindingIdentifier(identifier) = &property.value.kind
            && let Some(symbol_id) = identifier.symbol_id.get()
        {
            self.unsafe_symbols.insert(symbol_id);
        }
        oxc_ast::visit::walk::walk_binding_property(self, property);
    }
}

struct SourceRenameSpanCollector<'b> {
    symbol_renames: &'b BTreeMap<SymbolId, String>,
    reference_renames: &'b BTreeMap<ReferenceId, String>,
    replacements: Vec<(usize, usize, String)>,
}

impl<'a> Visit<'a> for SourceRenameSpanCollector<'_> {
    fn visit_binding_identifier(&mut self, identifier: &BindingIdentifier<'a>) {
        let Some(symbol_id) = identifier.symbol_id.get() else {
            return;
        };
        let Some(renamed) = self.symbol_renames.get(&symbol_id) else {
            return;
        };
        self.replacements.push((
            identifier.span.start as usize,
            identifier.span.end as usize,
            renamed.clone(),
        ));
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        let Some(reference_id) = identifier.reference_id.get() else {
            return;
        };
        let Some(renamed) = self.reference_renames.get(&reference_id) else {
            return;
        };
        self.replacements.push((
            identifier.span.start as usize,
            identifier.span.end as usize,
            renamed.clone(),
        ));
    }
}

fn apply_source_replacements(
    source: &str,
    mut replacements: Vec<(usize, usize, String)>,
) -> String {
    replacements.sort_by_key(|(start, end, _)| (*start, *end));
    replacements.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0_usize;
    for (start, end, replacement) in replacements {
        if start < cursor {
            continue;
        }
        output.push_str(&source[cursor..start]);
        output.push_str(replacement.as_str());
        cursor = end;
    }
    output.push_str(&source[cursor..]);
    output
}

fn source_preserving_rename_is_valid(
    source: &str,
    source_type: oxc_span::SourceType,
    expected_template_raws: &[String],
) -> bool {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type)
        .with_options(parse_options_for(source_type))
        .parse();
    if !parsed.errors.is_empty() || parsed.panicked {
        return false;
    }
    collect_template_raws(&parsed.program) == expected_template_raws
}

fn collect_template_raws(program: &Program<'_>) -> Vec<String> {
    let mut collector = TemplateRawCollector { raws: Vec::new() };
    collector.visit_program(program);
    collector.raws
}

struct TemplateRawCollector {
    raws: Vec<String>,
}

impl<'a> Visit<'a> for TemplateRawCollector {
    fn visit_template_element(&mut self, element: &TemplateElement<'a>) {
        self.raws.push(element.value.raw.as_str().to_string());
    }
}

fn should_assign_generated_semantic_name(
    name: &str,
    flags: SymbolFlags,
    is_exported_specifier: bool,
) -> bool {
    crate::identifier::is_minified_identifier(name)
        && !is_exported_specifier
        && !flags.intersects(SymbolFlags::Import | SymbolFlags::TypeImport)
        && !matches!(
            name,
            "_$cached"
                | "_$init"
                | "_$l"
                | "cmd"
                | "cwd"
                | "env"
                | "gid"
                | "pid"
                | "pkg"
                | "uid"
                | "uri"
        )
}

fn should_assign_generated_semantic_name_for_source_preservation(
    name: &str,
    flags: SymbolFlags,
    is_exported_specifier: bool,
    is_safe_import_symbol: bool,
) -> bool {
    crate::identifier::is_minified_identifier(name)
        && !is_exported_specifier
        && (!flags.intersects(SymbolFlags::Import | SymbolFlags::TypeImport)
            || is_safe_import_symbol)
        && !matches!(
            name,
            "_$cached"
                | "_$init"
                | "_$l"
                | "cmd"
                | "cwd"
                | "env"
                | "gid"
                | "pid"
                | "pkg"
                | "uid"
                | "uri"
        )
}

fn semantic_binding_name_base(flags: oxc_syntax::symbol::SymbolFlags) -> &'static str {
    if flags.is_function() {
        "semanticFunction"
    } else if flags.is_class() {
        "semanticClass"
    } else if flags.contains(oxc_syntax::symbol::SymbolFlags::CatchVariable) {
        "caughtError"
    } else if flags.contains(oxc_syntax::symbol::SymbolFlags::Import) {
        "semanticImport"
    } else if flags.is_type_parameter() {
        "semanticType"
    } else if flags.is_const_variable() {
        "semanticConstant"
    } else {
        "semanticValue"
    }
}

fn unique_safe_identifier_with_counters(
    base: &str,
    used_names: &mut BTreeSet<String>,
    next_suffix_by_base: &mut BTreeMap<String, usize>,
) -> String {
    if !next_suffix_by_base.contains_key(base) && !used_names.contains(base) {
        used_names.insert(base.to_string());
        next_suffix_by_base.insert(base.to_string(), 2);
        return base.to_string();
    }

    let next_suffix = next_suffix_by_base.entry(base.to_string()).or_insert(2);
    loop {
        let candidate = format!("{base}{next_suffix}");
        *next_suffix += 1;
        if used_names.insert(candidate.clone()) {
            return candidate;
        }
    }
}

fn unique_safe_identifier(base: &str, used_names: &mut BTreeSet<String>) -> String {
    if !used_names.contains(base) {
        used_names.insert(base.to_string());
        return base.to_string();
    }
    for suffix in 2.. {
        let candidate = format!("{base}{suffix}");
        if !used_names.contains(&candidate) {
            used_names.insert(candidate.clone());
            return candidate;
        }
    }
    unreachable!("unbounded suffix search should always find an identifier")
}

struct ReadabilityRenamer<'a> {
    builder: AstBuilder<'a>,
    symbol_renames: BTreeMap<SymbolId, String>,
    reference_renames: BTreeMap<ReferenceId, String>,
}

impl<'a> VisitMut<'a> for ReadabilityRenamer<'a> {
    fn visit_binding_identifier(&mut self, identifier: &mut BindingIdentifier<'a>) {
        let Some(symbol_id) = identifier.symbol_id.get() else {
            return;
        };
        let Some(renamed) = self.symbol_renames.get(&symbol_id) else {
            return;
        };
        identifier.name = self.builder.atom(renamed);
    }

    fn visit_identifier_reference(&mut self, identifier: &mut IdentifierReference<'a>) {
        let Some(reference_id) = identifier.reference_id.get() else {
            return;
        };
        let Some(renamed) = self.reference_renames.get(&reference_id) else {
            return;
        };
        identifier.name = self.builder.atom(renamed);
    }
}
