//! Per-subcommand implementations. Each module owns its arg parser,
//! runner, and any helpers that are not shared with other subcommands.
//! Cross-subcommand utilities (`next_path`, `next_value`,
//! `parse_project_id`, `format_audit_findings`) live in the crate root
//! and are exposed as `pub(crate)`.

pub(crate) mod binding_names;
pub(crate) mod coverage_ledger;
pub(crate) mod extract_assets;
pub(crate) mod full_inventory;
pub(crate) mod generate_project;
pub(crate) mod identifier_inventory;
pub(crate) mod import_unpacked;
pub(crate) mod match_modules;
pub(crate) mod match_packages;
pub(crate) mod module_classify;
pub(crate) mod naming_gates;
pub(crate) mod naming_plan;
pub(crate) mod naming_progress;
pub(crate) mod ownership_source_names;
pub(crate) mod package_cache;
pub(crate) mod package_surface_decisions;
pub(crate) mod package_version_diagnostics;
pub(crate) mod reference_source_names;
pub(crate) mod runtime_inventory;
pub(crate) mod symbol_index_io;
pub(crate) mod symbol_names;
