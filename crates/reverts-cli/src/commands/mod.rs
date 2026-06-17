//! Per-subcommand implementations. Each module owns its arg parser,
//! runner, and any helpers that are not shared with other subcommands.
//! Cross-subcommand utilities (`next_path`, `next_value`,
//! `parse_project_id`, `format_audit_findings`) live in the crate root
//! and are exposed as `pub(crate)`.

pub(crate) mod extract_assets;
pub(crate) mod generate_project;
pub(crate) mod match_packages;
pub(crate) mod package_cache;
pub(crate) mod package_version_diagnostics;
pub(crate) mod runtime_inventory;
pub(crate) mod symbol_names;
