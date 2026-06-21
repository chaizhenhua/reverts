//! SQLite persistence helpers for matcher / generator outputs.
//!
//! Each submodule owns one table family: its `persist_*` writer, any
//! schema-migration helpers, and the relevant `CREATE TABLE` /
//! `CREATE INDEX` SQL constants. Tables that the matcher merely reads
//! (handled by `reverts_input::sqlite`) stay there.

pub(crate) mod attributions;
pub(crate) mod externalization_hints;
pub(crate) mod fingerprint_cache;
pub(crate) mod function_attributions;
pub(crate) mod island_anchors;
pub(crate) mod island_package_candidates;
pub(crate) mod package_surfaces;
pub(crate) mod repository;
pub(crate) mod source_cache;
pub(crate) mod synthetic_modules;
