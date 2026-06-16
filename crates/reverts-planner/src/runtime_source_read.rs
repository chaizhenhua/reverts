//! Indexes a runtime helper file by where each binding is read or written.
//!
//! The setter-migration, lazy-fold and singleton-inline passes all need
//! to ask the same questions about a recovered runtime prelude:
//!
//! - Which snippets read this binding? Which write it?
//! - Which namespace-export getter forwards through this binding?
//! - Is this binding referenced anywhere *outside* a snippet (folded
//!   chunk, entrypoint side effect, namespace-export helper)?
//! - Is the entrypoint's callee one of these bindings?
//!
//! `RuntimeSourceReadIndex` precomputes those answers once per prelude so
//! the per-binding queries are O(1) hash/btreemap lookups. The reads are
//! split into several buckets so callers can ask "is this binding read
//! from a lazy-safe context only?" without re-walking the source.
//!
//! `runtime_source_read_index` is the constructor. It walks each
//! snippet's source for reads/writes, walks each folded chunk for
//! lazy-safe vs. unsafe references, and collates the namespace-export
//! shapes into the four `namespace_*` indexes.

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::{RuntimeNamespaceExport, RuntimePrelude};
use reverts_ir::BindingName;

use crate::{
    RuntimeFoldedSourceChunk, RuntimeSetterMigrationBlockerReason, identifier_read_facts_in_source,
    implicit_global_writes_in_source, local_bindings_in_source,
    lowered_lazy_initializer_statement_binding, runtime_import_identifiers_in_source,
    top_level_statement_slices,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeSourceReadIndex {
    pub(crate) snippet_readers_by_binding: BTreeMap<BindingName, BTreeSet<BindingName>>,
    pub(crate) snippet_writers_by_binding: BTreeMap<BindingName, BTreeSet<BindingName>>,
    pub(crate) namespace_readers_by_binding: BTreeMap<BindingName, BTreeSet<BindingName>>,
    pub(crate) namespace_exports_by_namespace: BTreeMap<BindingName, RuntimeNamespaceExport>,
    pub(crate) namespace_export_helpers: BTreeSet<BindingName>,
    pub(crate) free_bindings_by_snippet: BTreeMap<BindingName, BTreeSet<BindingName>>,
    pub(crate) non_snippet_runtime_reads: BTreeSet<BindingName>,
    pub(crate) folded_non_snippet_runtime_reads: BTreeSet<BindingName>,
    pub(crate) folded_lazy_safe_runtime_reads: BTreeSet<BindingName>,
    pub(crate) folded_non_call_runtime_reads: BTreeSet<BindingName>,
    pub(crate) folded_unsafe_runtime_reads: BTreeSet<BindingName>,
    pub(crate) entrypoint_non_snippet_runtime_reads: BTreeSet<BindingName>,
    pub(crate) entrypoint_callee: Option<BindingName>,
}

pub(crate) fn runtime_source_read_index(
    prelude: &RuntimePrelude,
    folded_chunks: &[RuntimeFoldedSourceChunk],
) -> RuntimeSourceReadIndex {
    let mut index = RuntimeSourceReadIndex {
        entrypoint_callee: prelude
            .entrypoint
            .as_ref()
            .map(|entrypoint| entrypoint.callee.clone()),
        ..RuntimeSourceReadIndex::default()
    };
    for (key, snippet) in &prelude.snippets {
        let local_bindings = local_bindings_in_source(snippet.source.as_str());
        let free_bindings = runtime_import_identifiers_in_source(snippet.source.as_str())
            .into_iter()
            .map(BindingName::new)
            .collect::<BTreeSet<_>>();
        for binding in &free_bindings {
            index
                .snippet_readers_by_binding
                .entry(binding.clone())
                .or_default()
                .insert(key.clone());
        }
        index
            .free_bindings_by_snippet
            .insert(key.clone(), free_bindings);
        for write in implicit_global_writes_in_source(snippet.source.as_str())
            .into_iter()
            .filter(|write| !local_bindings.contains(write.as_str()))
            .filter(|write| prelude.defines(write))
        {
            index
                .snippet_writers_by_binding
                .entry(write)
                .or_default()
                .insert(key.clone());
        }
    }

    for chunk in folded_chunks {
        let folded_reads = runtime_import_identifiers_in_source(chunk.source.as_str())
            .into_iter()
            .map(BindingName::new)
            .collect::<BTreeSet<_>>();
        index
            .non_snippet_runtime_reads
            .extend(folded_reads.iter().cloned());
        index.folded_non_snippet_runtime_reads.extend(folded_reads);
        for statement in top_level_statement_slices(chunk.source.as_str()) {
            let facts = identifier_read_facts_in_source(statement)
                .into_iter()
                .map(|fact| (BindingName::new(fact.name.as_str()), fact.is_call_callee))
                .collect::<Vec<_>>();
            if facts.is_empty() {
                continue;
            }
            if lowered_lazy_initializer_statement_binding(statement).is_some() {
                index
                    .folded_lazy_safe_runtime_reads
                    .extend(facts.into_iter().map(|(binding, _is_call)| binding));
                continue;
            }
            for (binding, is_call) in facts {
                if is_call {
                    index.folded_unsafe_runtime_reads.insert(binding);
                } else {
                    index.folded_non_call_runtime_reads.insert(binding);
                }
            }
        }
    }

    if let Some(entrypoint) = &prelude.entrypoint {
        for side_effect in &entrypoint.side_effects {
            let entrypoint_reads =
                runtime_import_identifiers_in_source(side_effect.source.as_str())
                    .into_iter()
                    .map(BindingName::new)
                    .collect::<BTreeSet<_>>();
            index
                .non_snippet_runtime_reads
                .extend(entrypoint_reads.iter().cloned());
            index
                .entrypoint_non_snippet_runtime_reads
                .extend(entrypoint_reads);
        }
    }

    for namespace_export in &prelude.namespace_exports {
        index
            .namespace_export_helpers
            .insert(namespace_export.helper.clone());
        index
            .namespace_exports_by_namespace
            .insert(namespace_export.namespace.clone(), namespace_export.clone());
        for target in namespace_export.exports.values() {
            index
                .namespace_readers_by_binding
                .entry(target.clone())
                .or_default()
                .insert(namespace_export.namespace.clone());
        }
    }

    index
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeBindingReadProfile {
    NoReads,
    SnippetReaders(BTreeSet<BindingName>),
    Rejected,
}

pub(crate) fn runtime_binding_read_profile(
    read_index: &RuntimeSourceReadIndex,
    binding: &BindingName,
) -> RuntimeBindingReadProfile {
    if runtime_binding_read_profile_diagnostic(read_index, binding).is_err() {
        return RuntimeBindingReadProfile::Rejected;
    }

    let readers = runtime_readers_for_binding(read_index, binding);
    if readers.is_empty() {
        RuntimeBindingReadProfile::NoReads
    } else {
        RuntimeBindingReadProfile::SnippetReaders(readers)
    }
}

pub(crate) fn runtime_binding_read_profile_diagnostic(
    read_index: &RuntimeSourceReadIndex,
    binding: &BindingName,
) -> Result<RuntimeBindingReadProfile, RuntimeSetterMigrationBlockerReason> {
    if read_index.non_snippet_runtime_reads.contains(binding) {
        return Err(RuntimeSetterMigrationBlockerReason::RuntimeNonSnippetRead);
    }
    if read_index.namespace_export_helpers.contains(binding) {
        return Err(RuntimeSetterMigrationBlockerReason::RuntimeNamespaceExportHelper);
    }
    if read_index
        .namespace_exports_by_namespace
        .contains_key(binding)
    {
        return Err(RuntimeSetterMigrationBlockerReason::RuntimeNamespaceObjectBinding);
    }

    let readers = runtime_readers_for_binding(read_index, binding);
    if readers.is_empty() {
        Ok(RuntimeBindingReadProfile::NoReads)
    } else {
        Ok(RuntimeBindingReadProfile::SnippetReaders(readers))
    }
}

pub(crate) fn runtime_readers_for_binding(
    read_index: &RuntimeSourceReadIndex,
    binding: &BindingName,
) -> BTreeSet<BindingName> {
    let mut readers = read_index
        .snippet_readers_by_binding
        .get(binding)
        .cloned()
        .unwrap_or_default();
    readers.extend(
        read_index
            .namespace_readers_by_binding
            .get(binding)
            .into_iter()
            .flatten()
            .cloned(),
    );
    readers
}
