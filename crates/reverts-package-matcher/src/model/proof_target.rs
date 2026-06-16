#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalImportTarget {
    pub(crate) export_specifier: String,
    pub(crate) source_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConcretePackageSourcePath {
    pub(crate) package_name: String,
    pub(crate) package_version: String,
    pub(crate) source_path: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CorrectedPackageExternalImportTarget {
    pub(crate) package_name: String,
    pub(crate) package_version: String,
    pub(crate) target: ExternalImportTarget,
    pub(crate) function_signature_matches: usize,
    pub(crate) string_anchor_matches: usize,
}
