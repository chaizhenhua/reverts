//! Package-runtime helper usage accumulated during module planning.

use std::collections::BTreeMap;

use crate::package_runtime::{PackageRuntimeHelperKey, PackageRuntimeHelperUsage};

/// Package-runtime helper usage discovered during module planning.
#[derive(Default)]
pub(crate) struct PackageRuntimeAccumulator {
    pub(crate) used_helper_files: BTreeMap<PackageRuntimeHelperKey, PackageRuntimeHelperUsage>,
}
