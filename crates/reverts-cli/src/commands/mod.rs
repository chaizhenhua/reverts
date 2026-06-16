//! Per-subcommand implementations. Each module owns its arg parser,
//! runner, and any helpers that are not shared with other subcommands.
//! Cross-subcommand utilities (`next_path`, `next_value`,
//! `parse_project_id`, `format_audit_findings`) live in the crate root
//! and are exposed as `pub(crate)`.

pub(crate) mod generate_project;
pub(crate) mod runtime_inventory;
