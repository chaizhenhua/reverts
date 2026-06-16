//! SQLite persistence helpers for matcher / generator outputs.
//!
//! Each submodule owns one table family: its `persist_*` writer, any
//! schema-migration helpers, and the relevant `CREATE TABLE` /
//! `CREATE INDEX` SQL constants. Tables that the matcher merely reads
//! (handled by `reverts_input::sqlite`) stay there.

pub(crate) mod function_attributions;
pub(crate) mod package_surfaces;
pub(crate) mod synthetic_modules;
