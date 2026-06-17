#[derive(Debug, Clone)]
pub struct ModuleRow {
    pub id: i64,
    pub category: String,
    pub package_name: Option<String>,
    pub package_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AttributionRow {
    pub module_id: i64,
    pub package_name: String,
    pub package_version: Option<String>,
    pub export_specifier: Option<String>,
    pub emission_mode: String,
    pub status: String,
    pub evidence_json: Option<String>,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HintRow {
    pub package_name: String,
    pub package_version: String,
    pub export_specifier: String,
    pub public_members: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub modules: Vec<ModuleRow>,
    pub attributions: Vec<AttributionRow>,
    pub hints: Vec<HintRow>,
}
