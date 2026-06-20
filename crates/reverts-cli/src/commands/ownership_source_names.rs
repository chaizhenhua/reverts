//! `ownership-source-names` command: name the functions of package-owned-but-
//! not-externalized modules from their matched npm package source.
//!
//! The package matcher records module->package@version "ownership" matches that
//! it cannot safely turn into an external `import` (the inlined esbuild bundle
//! does not prove a clean single import), so those attributions are persisted as
//! `rejected` and otherwise discarded. Those modules nonetheless ARE the
//! published source of a known package, so the package source file is an
//! authoritative naming reference. This command loads each owned module's
//! matched package source from the global cache and recovers real function
//! names — completely independent of externalization.

use std::path::PathBuf;

use clap::Args;

use crate::args::{parse_args_with_name, parse_project_id};
use crate::commands::reference_source_names::{OwnershipNamingRequest, run_ownership_source_names};
use crate::errors::{CliError, CliRunError};

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct OwnershipSourceNamesArgs {
    /// Per-run SQLite input database (holds `package_attributions`).
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    /// Global package source cache database. Defaults to `$HOME/.reverts/.reverts.db`.
    #[arg(long)]
    pub cache_db: Option<PathBuf>,
    /// Persist recovered names; without it, dry-run only.
    #[arg(long, default_value_t = false)]
    pub apply: bool,
    /// Non-automated origin prefix stored with accepted names. Keep it
    /// non-automated so the vocabulary gate stays bypassed and package domain
    /// names (`parseSemVer`, `useState`) are accepted on identifier evidence.
    #[arg(long, default_value = "package-source")]
    pub origin_prefix: String,
}

impl OwnershipSourceNamesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|a| a == crate::help::OWNERSHIP_SOURCE_NAMES_COMMAND)
        {
            args.remove(0);
        }
        parse_args_with_name(crate::help::OWNERSHIP_SOURCE_NAMES_COMMAND, args)
    }
}

/// Resolve the package source cache database path: the `--cache-db` override if
/// set, else `$HOME/.reverts/.reverts.db`.
fn resolve_cache_db(args: &OwnershipSourceNamesArgs) -> Result<PathBuf, CliRunError> {
    if let Some(path) = &args.cache_db {
        return Ok(path.clone());
    }
    let home = std::env::var("HOME").map_err(|_| {
        CliRunError::ReferenceSourceNames(
            "HOME is not set; pass --cache-db to locate the package source cache".to_string(),
        )
    })?;
    Ok(PathBuf::from(home).join(".reverts").join(".reverts.db"))
}

pub(crate) fn run(args: OwnershipSourceNamesArgs) -> Result<(), CliRunError> {
    let cache_db = resolve_cache_db(&args)?;
    let request = OwnershipNamingRequest {
        input: args.input.to_string_lossy().into_owned(),
        project_id: args.project_id,
        cache_db,
        apply: args.apply,
        origin_prefix: args.origin_prefix,
    };
    run_ownership_source_names(&request)
}
