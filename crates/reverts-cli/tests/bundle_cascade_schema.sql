CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE source_files (id INTEGER PRIMARY KEY, file_path TEXT NOT NULL);
CREATE TABLE project_files (project_id INTEGER NOT NULL, file_id INTEGER NOT NULL);
CREATE TABLE modules (
    id INTEGER PRIMARY KEY,
    file_id INTEGER,
    original_name TEXT NOT NULL,
    semantic_name TEXT,
    module_category TEXT,
    package_name TEXT,
    package_version TEXT,
    byte_start INTEGER,
    byte_end INTEGER
);
CREATE TABLE symbols (
    module_id INTEGER,
    semantic_name TEXT,
    export_name TEXT,
    original_name TEXT,
    scope_level TEXT
);
CREATE TABLE module_dependencies (module_id INTEGER, dependency_id INTEGER);
CREATE TABLE package_source_cache (
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    entry_path TEXT NOT NULL,
    source_content TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    fetched_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    PRIMARY KEY (package_name, package_version, entry_path)
);
CREATE TABLE package_attributions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id INTEGER NOT NULL,
    module_original_name TEXT NOT NULL,
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    package_subpath TEXT,
    resolved_file TEXT,
    export_specifier TEXT,
    emission_mode TEXT NOT NULL,
    status TEXT NOT NULL,
    evidence_json TEXT,
    rejection_reason TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (module_id)
);
