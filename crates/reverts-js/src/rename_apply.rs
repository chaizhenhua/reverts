use std::collections::{BTreeMap, BTreeSet};

use oxc_allocator::Allocator;
use oxc_ast::{
    AstBuilder, VisitMut,
    ast::{BindingIdentifier, IdentifierReference, Program},
};
use oxc_semantic::SemanticBuilder;
use oxc_syntax::{reference::ReferenceId, symbol::SymbolId};

use crate::identifier::sanitize_identifier;
use crate::{GeneratedRename, ReadabilityReport};

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

pub(crate) fn apply_all_scope_readability_renames<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    readability_renames: &[GeneratedRename],
    report: &mut ReadabilityReport,
) {
    let requested = readability_renames
        .iter()
        .map(|rename| (rename.original.clone(), rename.renamed.clone()))
        .collect::<BTreeMap<_, _>>();
    if requested.is_empty() {
        return;
    }

    let (symbol_renames, reference_renames) = {
        let semantic = SemanticBuilder::new().build(program).semantic;
        let symbols = semantic.symbols();
        let symbol_ids = symbols.symbol_ids().collect::<Vec<_>>();
        let mut symbol_names_by_scope = BTreeMap::<_, BTreeMap<String, Vec<SymbolId>>>::new();
        for symbol_id in &symbol_ids {
            symbol_names_by_scope
                .entry(symbols.get_scope_id(*symbol_id))
                .or_default()
                .entry(symbols.get_name(*symbol_id).to_string())
                .or_default()
                .push(*symbol_id);
        }

        let mut requested_targets_by_scope = BTreeMap::<_, BTreeSet<String>>::new();
        let mut symbol_renames = BTreeMap::<SymbolId, String>::new();
        for symbol_id in symbol_ids {
            let original = symbols.get_name(symbol_id);
            let Some(renamed) = requested.get(original) else {
                continue;
            };
            let scope_id = symbols.get_scope_id(symbol_id);
            let same_scope_names = symbol_names_by_scope.entry(scope_id).or_default();
            let collides = same_scope_names
                .get(renamed)
                .is_some_and(|ids| ids.iter().any(|id| *id != symbol_id));
            if collides {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source=explicit_binding_semantic, reason=name_collision"
                ));
                continue;
            }
            if !requested_targets_by_scope
                .entry(scope_id)
                .or_default()
                .insert(renamed.clone())
            {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source=explicit_binding_semantic, reason=duplicate_target"
                ));
                continue;
            }
            symbol_renames.insert(symbol_id, renamed.clone());
            report.push(format!(
                "renamed {original} -> {renamed}, source=explicit_binding_semantic"
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
