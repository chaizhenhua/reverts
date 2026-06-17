//! Help topics and rendered help text for the `reverts-cli` binary. Kept
//! separate from the parser/runner so that updating one piece of help copy
//! does not force a rebuild of the rest of the CLI module tree.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTopic {
    TopLevel,
    GenerateProjectV2,
    MatchPackages,
    MatchPackagesReport,
    PackageVersionDiagnostics,
    PackageCacheAudit,
    PackageCachePruneStale,
    PackageExternalizationHints,
    ExtractAssets,
    RuntimeInventory,
    SymbolNames,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub topic: HelpTopic,
    pub summary: &'static str,
}

pub const GENERATE_PROJECT_V2_COMMAND: &str = "generate-project-v2";
pub const MATCH_PACKAGES_COMMAND: &str = "match-packages";
pub const MATCH_PACKAGES_REPORT_COMMAND: &str = "match-packages-report";
pub const PACKAGE_VERSION_DIAGNOSTICS_COMMAND: &str = "package-version-diagnostics";
pub const PACKAGE_CACHE_AUDIT_COMMAND: &str = "package-cache-audit";
pub const PACKAGE_CACHE_PRUNE_STALE_COMMAND: &str = "package-cache-prune-stale";
pub const PACKAGE_EXTERNALIZATION_HINTS_COMMAND: &str = "package-externalization-hints";
pub const EXTRACT_ASSETS_COMMAND: &str = "extract-assets";
pub const RUNTIME_INVENTORY_COMMAND: &str = "runtime-inventory";
pub const SYMBOL_NAMES_COMMAND: &str = "symbol-names";

pub const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        name: MATCH_PACKAGES_COMMAND,
        topic: HelpTopic::MatchPackages,
        summary: "Populate package_attributions/package_surfaces in SQLite",
    },
    CommandSpec {
        name: MATCH_PACKAGES_REPORT_COMMAND,
        topic: HelpTopic::MatchPackagesReport,
        summary: "Report package match, externalization, and source-elimination rates across projects",
    },
    CommandSpec {
        name: PACKAGE_VERSION_DIAGNOSTICS_COMMAND,
        topic: HelpTopic::PackageVersionDiagnostics,
        summary: "Diagnose rejected package-version matches without writing SQLite",
    },
    CommandSpec {
        name: PACKAGE_CACHE_AUDIT_COMMAND,
        topic: HelpTopic::PackageCacheAudit,
        summary: "Audit package_source_cache freshness and validity",
    },
    CommandSpec {
        name: PACKAGE_CACHE_PRUNE_STALE_COMMAND,
        topic: HelpTopic::PackageCachePruneStale,
        summary: "Delete invalid/stale package_source_cache rows with --apply",
    },
    CommandSpec {
        name: PACKAGE_EXTERNALIZATION_HINTS_COMMAND,
        topic: HelpTopic::PackageExternalizationHints,
        summary: "Generate verified package externalization hint rows",
    },
    CommandSpec {
        name: EXTRACT_ASSETS_COMMAND,
        topic: HelpTopic::ExtractAssets,
        summary: "Populate project_assets from asset references in source slices",
    },
    CommandSpec {
        name: GENERATE_PROJECT_V2_COMMAND,
        topic: HelpTopic::GenerateProjectV2,
        summary: "Generate a TypeScript project from SQLite input",
    },
    CommandSpec {
        name: RUNTIME_INVENTORY_COMMAND,
        topic: HelpTopic::RuntimeInventory,
        summary: "Measure emitted runtime helpers and generated internal names",
    },
    CommandSpec {
        name: SYMBOL_NAMES_COMMAND,
        topic: HelpTopic::SymbolNames,
        summary: "List or manually set symbol semantic names in SQLite",
    },
];

#[must_use]
pub fn command_topic(command: &str) -> Option<HelpTopic> {
    COMMAND_SPECS
        .iter()
        .find(|spec| spec.name == command)
        .map(|spec| spec.topic)
}

#[must_use]
pub fn version_text() -> String {
    format!("reverts-cli {}", env!("CARGO_PKG_VERSION"))
}

#[must_use]
pub fn help_text(topic: HelpTopic) -> &'static str {
    match topic {
        HelpTopic::TopLevel => {
            "reverts-cli\n\nUSAGE:\n    reverts-cli <COMMAND> [OPTIONS]\n    reverts-cli --help [COMMAND]\n    reverts-cli --version\n\nCOMMANDS:\n    match-packages                   Populate package_attributions/package_surfaces in SQLite\n    match-packages-report            Report package match, externalization, and source-elimination rates across projects\n    package-version-diagnostics      Diagnose rejected package-version matches without writing SQLite\n    package-cache-audit              Audit package_source_cache freshness and validity\n    package-cache-prune-stale        Delete invalid/stale package_source_cache rows with --apply\n    package-externalization-hints    Generate verified package externalization hint rows\n    extract-assets                   Populate project_assets from asset references in source slices\n    generate-project-v2              Generate a TypeScript project from SQLite input\n    runtime-inventory                Measure emitted runtime helpers and generated internal names\n    symbol-names                     List, propose, or accept symbol semantic names in SQLite\n\nUse `reverts-cli help <COMMAND>` for command-specific help."
        }
        HelpTopic::GenerateProjectV2 => {
            "reverts-cli generate-project-v2\n\nUSAGE:\n    reverts-cli generate-project-v2 --input <DB> --project-id <ID> --output <DIR>\n\nOPTIONS:\n    --input <DB>          SQLite input database\n    --project-id <ID>     Positive project id\n    --output <DIR>        Output directory for the generated TypeScript project"
        }
        HelpTopic::MatchPackages => {
            "reverts-cli match-packages\n\nUSAGE:\n    reverts-cli match-packages --input <DB> --project-id <ID> [--package-name <NAME> ...] [--package-source-root <DIR> ...] [--materialize-package-sources] [--apply]\n\nOPTIONS:\n    --input <DB>                     SQLite input database\n    --project-id <ID>                Positive project id\n    --package-name <NAME>            Restrict matching to the package graph component containing this package; repeatable\n    --package-source-root <DIR>      Additional local package source root (package dir, node_modules, or project root containing node_modules); repeatable. Loaded files are source-only unless later proven importable.\n    --materialize-package-sources    Resolve exact/range/missing package version hints and npm-install only concrete, compatible package versions into a temporary source root before matching; with --apply, persist collected sources to package_source_cache\n    --apply                          Persist accepted package attributions, surfaces, and materialized package source cache rows"
        }
        HelpTopic::MatchPackagesReport => {
            "reverts-cli match-packages-report\n\nUSAGE:\n    reverts-cli match-packages-report --input <DB> --all-projects [--limit <N>] [--newest] [--package-name <NAME> ...] [--package-source-root <DIR> ...] [--materialize-package-sources]\n\nOPTIONS:\n    --input <DB>                     SQLite input database\n    --all-projects                   Inspect every project id in the database\n    --limit <N>                      Maximum number of project ids to inspect\n    --newest                         Visit highest project ids first\n    --package-name <NAME>            Restrict matching to the package graph component containing this package; repeatable\n    --package-source-root <DIR>      Additional local package source root; repeatable\n    --materialize-package-sources    Resolve and npm-install concrete package versions in-memory for the report\n\nMETRICS:\n    direct_externalized              Package modules emitted as direct package imports\n    private_source_suppressed        Private package modules safely removed with an externalized closure\n    source_eliminated                direct_externalized + private_source_suppressed\n    source_remaining                 Package source modules still requiring source preservation"
        }
        HelpTopic::PackageVersionDiagnostics => {
            "reverts-cli package-version-diagnostics\n\nUSAGE:\n    reverts-cli package-version-diagnostics --input <DB> --project-id <ID> [--package-name <NAME> ...] [--package-source-root <DIR> ...] [--materialize-package-sources] [--top <N>]\n\nOPTIONS:\n    --input <DB>                     SQLite input database (opened read-only)\n    --project-id <ID>                Positive project id\n    --package-name <NAME>            Restrict diagnostics to one package; repeatable\n    --package-source-root <DIR>      Additional local package source root; repeatable\n    --materialize-package-sources    Resolve and npm-install candidate package sources in-memory only; never writes SQLite\n    --top <N>                        Candidate versions to print per package (default: 5)\n\nDIAGNOSTIC:\n    Inspects rejected package_attributions whose reason is selected package version did not match this module source, treats DB versions as hints, and scores cached/source-root candidate versions against module source fingerprints without applying changes."
        }
        HelpTopic::PackageCacheAudit => {
            "reverts-cli package-cache-audit\n\nUSAGE:\n    reverts-cli package-cache-audit --input <DB>\n\nOPTIONS:\n    --input <DB>    SQLite input database"
        }
        HelpTopic::PackageCachePruneStale => {
            "reverts-cli package-cache-prune-stale\n\nUSAGE:\n    reverts-cli package-cache-prune-stale --input <DB> [--apply]\n\nOPTIONS:\n    --input <DB>    SQLite input database\n    --apply         Delete invalid or stale package_source_cache rows; without --apply, only prints what would be deleted"
        }
        HelpTopic::PackageExternalizationHints => {
            "reverts-cli package-externalization-hints\n\nUSAGE:\n    reverts-cli package-externalization-hints --input <DB> [--package-name <NAME> ...] [--limit <N>] [--apply]\n\nOPTIONS:\n    --input <DB>            SQLite input database\n    --package-name <NAME>   Restrict hint generation to one package name; repeatable\n    --limit <N>             Maximum number of verified cache rows to inspect\n    --apply                 Persist generated hints into package_externalization_hints; without --apply, only prints what would be written"
        }
        HelpTopic::ExtractAssets => {
            "reverts-cli extract-assets\n\nUSAGE:\n    reverts-cli extract-assets --input <DB> --project-id <ID> [--asset-root <DIR-OR-BUN-EXE>]... [--apply]\n\nOPTIONS:\n    --input <DB>                    SQLite input database\n    --project-id <ID>               Positive project id\n    --asset-root <DIR-OR-BUN-EXE>   Root directory for asset files, or a Bun standalone executable for /$bunfs/root assets (repeatable)\n    --apply                         Persist discovered project_assets rows"
        }
        HelpTopic::RuntimeInventory => {
            "reverts-cli runtime-inventory\n\nUSAGE:\n    reverts-cli runtime-inventory --input <DB> (--project-id <ID> | --all-projects) [--limit <N>] [--newest] [--max-source-bytes <N>] [--setter-blockers] [--runtime-attribution]\n\nOPTIONS:\n    --input <DB>                SQLite input database\n    --project-id <ID>           Positive project id to inspect\n    --all-projects              Inspect every project id in the database\n    --limit <N>                 Maximum number of project ids to inspect when --all-projects is used\n    --newest                    Visit highest project ids first when --all-projects is used\n    --max-source-bytes <N>      Skip projects whose source_files total exceeds this byte limit\n    --setter-blockers           Also print conservative runtime setter migration blocker distribution\n    --runtime-attribution       Attribute emitted runtime helper lines by top-level binding and kind"
        }
        HelpTopic::SymbolNames => {
            "reverts-cli symbol-names\n\nUSAGE:\n    reverts-cli symbol-names --input <DB> --project-id <ID> --list [--all-proposals]\n    reverts-cli symbol-names --input <DB> --project-id <ID> [--propose <MODULE_ID:ORIGINAL=SEMANTIC> ...] [--accept <MODULE_ID:ORIGINAL=SEMANTIC> ...] [--clear-active <MODULE_ID:ORIGINAL> ...] [--origin <SOURCE>] [--evidence <TEXT>] [--batch <TSV|->] [--apply]\n\nOPTIONS:\n    --input <DB>          SQLite input database\n    --project-id <ID>     Positive project id\n    --list                Print active module/global symbols as TSV\n    --all-proposals       With --list, print recorded name proposals instead of active symbols\n    --propose <SPEC>      Record a naming proposal without changing emitted output; repeatable\n    --accept <SPEC>       Record and activate a semantic name for the next emit; repeatable (--set alias)\n    --clear-active <SPEC> Clear the active semantic name; repeatable (--clear alias)\n    --origin <SOURCE>     Proposal source label, default: agent\n    --evidence <TEXT>     Optional evidence stored with proposals from this invocation\n    --batch <TSV|->       Read tab-separated propose/accept/clear-active operations from a file or stdin\n    --apply               Persist changes; without --apply, only validates and prints a dry-run summary\n\nBATCH TSV:\n    propose<TAB>module_id<TAB>original_name<TAB>semantic_name\n    accept<TAB>module_id<TAB>original_name<TAB>semantic_name\n    clear-active<TAB>module_id<TAB>original_name"
        }
    }
}
