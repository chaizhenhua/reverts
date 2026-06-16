//! `extract-assets` command runner.

use crate::args::ExtractAssetsArgs;
use crate::errors::CliRunError;
use crate::extract_assets_from_sqlite;

pub(crate) fn run(args: ExtractAssetsArgs) -> Result<(), CliRunError> {
    let outcome = extract_assets_from_sqlite(&args).map_err(CliRunError::ExtractAssets)?;
    println!(
        "extracted assets for project {}: {} reference(s), {} matched, {} missing, {} written",
        outcome.project_id,
        outcome.referenced_assets,
        outcome.matched_assets,
        outcome.missing_assets,
        outcome.written_assets
    );
    Ok(())
}
