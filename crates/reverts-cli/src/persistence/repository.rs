//! Persistence adapter facade for CLI workflows.
//!
//! The table-specific modules still own SQL and migrations. This facade is the
//! first hexagonal boundary: command code talks to a persistence port, while the
//! SQLite adapter coordinates multi-table writes and FK-related filtering.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{InputRows, PackageAttributionInput};
use reverts_ir::ModuleId;
use reverts_package_matcher::VersionedPackageMatchReport;
use rusqlite::Connection;

use crate::PackageVersionResolutionEvidence;
use crate::errors::MatchPackagesError;
use crate::persistence::{
    attributions, function_attributions, package_surfaces, synthetic_modules,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MatchPackagePersistenceOutcome {
    pub(crate) written_attributions: usize,
    pub(crate) written_surfaces: usize,
    pub(crate) written_function_attributions: usize,
}

pub(crate) trait MatchPackagePersistence {
    fn persist_match_package_outputs(
        &mut self,
        rows: &InputRows,
        synthetic_modules: &[reverts_input::ModuleInput],
        report: &VersionedPackageMatchReport,
        matched_package_names: &BTreeSet<String>,
        version_resolutions: &BTreeMap<ModuleId, PackageVersionResolutionEvidence>,
        function_attributions: &[PackageAttributionInput],
    ) -> Result<MatchPackagePersistenceOutcome, MatchPackagesError>;
}

pub(crate) struct SqliteMatchPackagePersistence<'a> {
    connection: &'a mut Connection,
}

impl<'a> SqliteMatchPackagePersistence<'a> {
    pub(crate) fn new(connection: &'a mut Connection) -> Self {
        Self { connection }
    }

    fn persisted_module_ids(&self) -> Result<BTreeSet<ModuleId>, MatchPackagesError> {
        let mut persisted_ids = BTreeSet::new();
        let mut stmt = self
            .connection
            .prepare("SELECT id FROM modules")
            .map_err(MatchPackagesError::WriteAttribution)?;
        let mut rows = stmt
            .query([])
            .map_err(MatchPackagesError::WriteAttribution)?;
        while let Some(row) = rows.next().map_err(MatchPackagesError::WriteAttribution)? {
            let id: u32 = row.get(0).map_err(MatchPackagesError::WriteAttribution)?;
            persisted_ids.insert(ModuleId(id));
        }
        Ok(persisted_ids)
    }
}

impl MatchPackagePersistence for SqliteMatchPackagePersistence<'_> {
    fn persist_match_package_outputs(
        &mut self,
        rows: &InputRows,
        synthetic_modules: &[reverts_input::ModuleInput],
        report: &VersionedPackageMatchReport,
        matched_package_names: &BTreeSet<String>,
        version_resolutions: &BTreeMap<ModuleId, PackageVersionResolutionEvidence>,
        function_attributions: &[PackageAttributionInput],
    ) -> Result<MatchPackagePersistenceOutcome, MatchPackagesError> {
        // Persist synthetic modules first so FKs from attribution tables resolve.
        synthetic_modules::persist_synthetic_modules(self.connection, synthetic_modules)?;

        // Some synthetic module inserts may be ignored due to legacy uniqueness
        // constraints. Keep FK filtering in the persistence adapter instead of
        // the command workflow.
        let persisted_ids = self.persisted_module_ids()?;
        let persistable_function_attributions = function_attributions
            .iter()
            .filter(|attribution| persisted_ids.contains(&attribution.module_id))
            .cloned()
            .collect::<Vec<_>>();

        Ok(MatchPackagePersistenceOutcome {
            written_attributions: attributions::persist_package_attributions(
                self.connection,
                rows,
                report,
                matched_package_names,
                version_resolutions,
            )?,
            written_surfaces: package_surfaces::persist_package_surfaces(
                self.connection,
                rows,
                report,
            )?,
            written_function_attributions: function_attributions::persist_function_attributions(
                self.connection,
                rows,
                &persistable_function_attributions,
            )?,
        })
    }
}
