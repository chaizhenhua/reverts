//! Per-binding diagnostic surface for the runtime-setter migration.
//!
//! The planner walks every candidate runtime-setter binding and either
//! migrates it to a module-local setter or refuses with a tagged reason.
//! `RuntimeSetterMigrationBlockerReport` is the report shape we hand back
//! to operators so they can see why a specific binding wasn't migrated
//! and aggregate failures across a project.
//!
//! The `Accepted` vs `Blocked(reason)` distinction is per-binding (keyed
//! by `(source_file_id, binding)`) rather than per-module so that a
//! module with mixed outcomes is faithfully represented — the planner
//! intentionally migrates partial subsets when that's safe.
//!
//! Sub-reasons (`sub_reasons`) are a secondary taxonomy used for blockers
//! that we want to refine without churning the public reason enum.
//! `ReaderNonSnippetUse` already has 7 sub-reasons mapped this way; new
//! refinements should go here too, not by adding new top-level variants.

use std::collections::BTreeMap;

use reverts_ir::BindingName;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RuntimeSetterMigrationBlockerReport {
    pub total_bindings: usize,
    pub accepted_bindings: usize,
    pub blocked_bindings: usize,
    pub reasons: BTreeMap<RuntimeSetterMigrationBlockerReason, usize>,
    /// Sub-reason distribution per top-level reason. Populated for
    /// ReaderNonSnippetUse (7 distinct internal causes) and any other
    /// blocker that carries a `sub_reason` label. Used for guard-
    /// relaxation planning without changing the top-level taxonomy.
    pub sub_reasons: BTreeMap<(RuntimeSetterMigrationBlockerReason, &'static str), usize>,
    pub binding_statuses:
        BTreeMap<RuntimeSetterMigrationBindingKey, RuntimeSetterMigrationBindingStatus>,
}

impl RuntimeSetterMigrationBlockerReport {
    pub fn add_accepted(&mut self, source_file_id: u32, binding: BindingName) {
        self.remove_existing_status(source_file_id, &binding);
        self.accepted_bindings += 1;
        self.binding_statuses.insert(
            RuntimeSetterMigrationBindingKey {
                source_file_id,
                binding,
            },
            RuntimeSetterMigrationBindingStatus::Accepted,
        );
    }

    pub fn add_reason(
        &mut self,
        source_file_id: u32,
        binding: BindingName,
        reason: RuntimeSetterMigrationBlockerReason,
    ) {
        self.add_reason_with_sub(source_file_id, binding, reason, None);
    }

    pub fn add_reason_with_sub(
        &mut self,
        source_file_id: u32,
        binding: BindingName,
        reason: RuntimeSetterMigrationBlockerReason,
        sub_reason: Option<&'static str>,
    ) {
        self.remove_existing_status(source_file_id, &binding);
        self.blocked_bindings += 1;
        *self.reasons.entry(reason).or_default() += 1;
        if let Some(label) = sub_reason {
            *self.sub_reasons.entry((reason, label)).or_default() += 1;
        }
        self.binding_statuses.insert(
            RuntimeSetterMigrationBindingKey {
                source_file_id,
                binding,
            },
            RuntimeSetterMigrationBindingStatus::Blocked(reason),
        );
    }

    pub fn add(&mut self, other: &Self) {
        self.total_bindings += other.total_bindings;
        self.accepted_bindings += other.accepted_bindings;
        self.blocked_bindings += other.blocked_bindings;
        self.binding_statuses.extend(
            other
                .binding_statuses
                .iter()
                .map(|(key, status)| (key.clone(), *status)),
        );
        for (reason, count) in &other.reasons {
            *self.reasons.entry(*reason).or_default() += count;
        }
        for (key, count) in &other.sub_reasons {
            *self.sub_reasons.entry(*key).or_default() += count;
        }
    }

    fn remove_existing_status(&mut self, source_file_id: u32, binding: &BindingName) {
        let key = RuntimeSetterMigrationBindingKey {
            source_file_id,
            binding: binding.clone(),
        };
        let Some(previous) = self.binding_statuses.remove(&key) else {
            return;
        };
        match previous {
            RuntimeSetterMigrationBindingStatus::Accepted => {
                self.accepted_bindings = self.accepted_bindings.saturating_sub(1);
            }
            RuntimeSetterMigrationBindingStatus::Blocked(reason) => {
                self.blocked_bindings = self.blocked_bindings.saturating_sub(1);
                if let Some(count) = self.reasons.get_mut(&reason) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        self.reasons.remove(&reason);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RuntimeSetterMigrationBindingKey {
    pub source_file_id: u32,
    pub binding: BindingName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSetterMigrationBindingStatus {
    Accepted,
    Blocked(RuntimeSetterMigrationBlockerReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimeSetterMigrationBlockerReason {
    MultipleEligibleWriters,
    FoldedWriterOnly,
    ExternalizedPackageWriterOnly,
    NoEligibleWriter,
    MissingRuntimePrelude,
    InitializerNotMigratable,
    RuntimeNonSnippetRead,
    RuntimeNamespaceExportHelper,
    RuntimeNamespaceObjectBinding,
    ReaderNonSnippetUse,
    ReaderSnippetMissing,
    ReaderNotMovableShape,
    ReaderWritesRuntimeBinding,
    ReaderClosureEscapes,
    ReaderFreeBindingIndexMissing,
    ReaderReadsOtherMovableBinding,
    ReaderReadsNonRuntimeBinding,
    NamespaceTargetDifferentWriter,
    OwnerSourceMissing,
    OwnerNameConflict,
    ReaderClusterOverlapsMigratedBinding,
    RuntimeReaderWriteSetterDependency,
    NoDiagnosticStatus,
}

impl RuntimeSetterMigrationBlockerReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MultipleEligibleWriters => "multiple_eligible_writers",
            Self::FoldedWriterOnly => "folded_writer_only",
            Self::ExternalizedPackageWriterOnly => "externalized_package_writer_only",
            Self::NoEligibleWriter => "no_eligible_writer",
            Self::MissingRuntimePrelude => "missing_runtime_prelude",
            Self::InitializerNotMigratable => "initializer_not_migratable",
            Self::RuntimeNonSnippetRead => "runtime_non_snippet_read",
            Self::RuntimeNamespaceExportHelper => "runtime_namespace_export_helper",
            Self::RuntimeNamespaceObjectBinding => "runtime_namespace_object_binding",
            Self::ReaderNonSnippetUse => "reader_non_snippet_use",
            Self::ReaderSnippetMissing => "reader_snippet_missing",
            Self::ReaderNotMovableShape => "reader_not_movable_shape",
            Self::ReaderWritesRuntimeBinding => "reader_writes_runtime_binding",
            Self::ReaderClosureEscapes => "reader_closure_escapes",
            Self::ReaderFreeBindingIndexMissing => "reader_free_binding_index_missing",
            Self::ReaderReadsOtherMovableBinding => "reader_reads_other_movable_binding",
            Self::ReaderReadsNonRuntimeBinding => "reader_reads_non_runtime_binding",
            Self::NamespaceTargetDifferentWriter => "namespace_target_different_writer",
            Self::OwnerSourceMissing => "owner_source_missing",
            Self::OwnerNameConflict => "owner_name_conflict",
            Self::ReaderClusterOverlapsMigratedBinding => {
                "reader_cluster_overlaps_migrated_binding"
            }
            Self::RuntimeReaderWriteSetterDependency => "runtime_reader_write_setter_dependency",
            Self::NoDiagnosticStatus => "no_diagnostic_status",
        }
    }
}
