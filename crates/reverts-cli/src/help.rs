//! Help topics and rendered help text for the `reverts-cli` binary. Kept
//! separate from the parser/runner so that updating one piece of help copy
//! does not force a rebuild of the rest of the CLI module tree.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTopic {
    TopLevel,
    GenerateProjectV2,
    ImportUnpacked,
    MatchPackages,
    MatchPackagesReport,
    PackageVersionDiagnostics,
    PackageCacheAudit,
    PackageCachePruneStale,
    PackageExternalizationHints,
    PackageSurfaceDecisions,
    ExtractAssets,
    FullInventory,
    CoverageLedger,
    IdentifierInventory,
    RuntimeInventory,
    SymbolNames,
    NamingProgress,
    NamingPlan,
    ModuleClassify,
    MatchModulesRecall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub topic: HelpTopic,
    pub summary: &'static str,
}

pub const GENERATE_PROJECT_V2_COMMAND: &str = "generate-project-v2";
pub const IMPORT_UNPACKED_COMMAND: &str = "import-unpacked";
pub const MATCH_PACKAGES_COMMAND: &str = "match-packages";
pub const MATCH_PACKAGES_REPORT_COMMAND: &str = "match-packages-report";
pub const PACKAGE_VERSION_DIAGNOSTICS_COMMAND: &str = "package-version-diagnostics";
pub const PACKAGE_CACHE_AUDIT_COMMAND: &str = "package-cache-audit";
pub const PACKAGE_CACHE_PRUNE_STALE_COMMAND: &str = "package-cache-prune-stale";
pub const PACKAGE_EXTERNALIZATION_HINTS_COMMAND: &str = "package-externalization-hints";
pub const PACKAGE_SURFACE_DECISIONS_COMMAND: &str = "package-surface-decisions";
pub const EXTRACT_ASSETS_COMMAND: &str = "extract-assets";
pub const FULL_INVENTORY_COMMAND: &str = "full-inventory";
pub const COVERAGE_LEDGER_COMMAND: &str = "coverage-ledger";
pub const IDENTIFIER_INVENTORY_COMMAND: &str = "identifier-inventory";
pub const RUNTIME_INVENTORY_COMMAND: &str = "runtime-inventory";
pub const SYMBOL_NAMES_COMMAND: &str = "symbol-names";
pub const NAMING_PROGRESS_COMMAND: &str = "naming-progress";
pub const NAMING_PLAN_COMMAND: &str = "naming-plan";
pub const MODULE_CLASSIFY_COMMAND: &str = "module-classify";
pub const MATCH_MODULES_RECALL_COMMAND: &str = "match-modules-recall";

pub const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        name: IMPORT_UNPACKED_COMMAND,
        topic: HelpTopic::ImportUnpacked,
        summary: "Import unpack Skill evidence into Reverts SQLite facts",
    },
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
        name: PACKAGE_SURFACE_DECISIONS_COMMAND,
        topic: HelpTopic::PackageSurfaceDecisions,
        summary: "Apply Agent-resolved package surface decisions",
    },
    CommandSpec {
        name: EXTRACT_ASSETS_COMMAND,
        topic: HelpTopic::ExtractAssets,
        summary: "Populate project_assets from asset references in source slices",
    },
    CommandSpec {
        name: FULL_INVENTORY_COMMAND,
        topic: HelpTopic::FullInventory,
        summary: "Write a full decompile inventory and coverage report",
    },
    CommandSpec {
        name: COVERAGE_LEDGER_COMMAND,
        topic: HelpTopic::CoverageLedger,
        summary: "Write the unified decompile coverage ledger",
    },
    CommandSpec {
        name: IDENTIFIER_INVENTORY_COMMAND,
        topic: HelpTopic::IdentifierInventory,
        summary: "Count AST identifier sites in generated JS/TS output",
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
    CommandSpec {
        name: NAMING_PROGRESS_COMMAND,
        topic: HelpTopic::NamingProgress,
        summary: "Report semantic-naming completion across public-surface/declarations/full tiers",
    },
    CommandSpec {
        name: NAMING_PLAN_COMMAND,
        topic: HelpTopic::NamingPlan,
        summary: "Emit the JSON work list of unnamed symbols (by tier) for a naming agent",
    },
    CommandSpec {
        name: MODULE_CLASSIFY_COMMAND,
        topic: HelpTopic::ModuleClassify,
        summary: "Classify modules (application/third-party/runtime-glue) to refine the naming denominator",
    },
    CommandSpec {
        name: MATCH_MODULES_RECALL_COMMAND,
        topic: HelpTopic::MatchModulesRecall,
        summary: "Measure cross-project module match recall against a ground-truth project",
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
            "reverts-cli\n\nUSAGE:\n    reverts-cli <COMMAND> [OPTIONS]\n    reverts-cli --help [COMMAND]\n    reverts-cli --version\n\nCOMMANDS:\n    import-unpacked                  Import unpack Skill evidence into Reverts SQLite facts\n    match-packages                   Populate package_attributions/package_surfaces in SQLite\n    match-packages-report            Report package match, externalization, and source-elimination rates across projects\n    package-version-diagnostics      Diagnose rejected package-version matches without writing SQLite\n    package-cache-audit              Audit package_source_cache freshness and validity\n    package-cache-prune-stale        Delete invalid/stale package_source_cache rows with --apply\n    package-externalization-hints    Generate verified package externalization hint rows\n    package-surface-decisions        Apply Agent-resolved package surface decisions\n    extract-assets                   Populate project_assets from asset references in source slices\n    generate-project-v2              Generate a TypeScript project from SQLite input\n    full-inventory                   Write a full decompile inventory and coverage report\n    coverage-ledger                  Write the unified decompile coverage ledger\n    identifier-inventory             Count AST identifier sites in every generated JS/TS output file\n    runtime-inventory                Measure emitted runtime helpers and generated internal names\n    symbol-names                     List, propose, or accept symbol semantic names in SQLite\n    naming-progress                  Report semantic-naming completion across public-surface/declarations/full tiers\n    naming-plan                      Emit the JSON work list of unnamed symbols (by tier) for a naming agent\n    module-classify                  Classify modules (application/third-party/runtime-glue) to refine the naming denominator\n    match-modules-recall             Measure cross-project module match recall against a ground-truth project\n\nUse `reverts-cli help <COMMAND>` for command-specific help."
        }
        HelpTopic::ImportUnpacked => {
            "reverts-cli import-unpacked\n\nUSAGE:\n    reverts-cli import-unpacked --input <UNPACKED_ROOT> --manifest <MANIFEST> --project-name <NAME> --output-db <DB> [--ignore-native-assets] [--max-source-bytes <N>] [--bundle-source-bytes <N>]\n\nOPTIONS:\n    --input <UNPACKED_ROOT>       Unpacked source root, for Electron usually Contents/Resources/app\n    --manifest <MANIFEST>         Authoritative reverts.import_evidence.v1 manifest; every input file must be covered and recorded size/hash evidence must match\n    --project-name <NAME>         Project name stored in Reverts SQLite\n    --output-db <DB>              SQLite database to create\n    --ignore-native-assets        Do not write native assets into project_assets after manifest validation\n    --max-source-bytes <N>        Defer source files larger than N bytes as project_assets instead of modules\n    --bundle-source-bytes <N>     Keep source files larger than N bytes as source_files without module rows so the pipeline can extract bundled modules\n\nOUTPUT:\n    Creates canonical Reverts facts: projects, source_files (with file_size), project_files, modules, module_dependencies, project_assets, and package_attributions."
        }
        HelpTopic::GenerateProjectV2 => {
            "reverts-cli generate-project-v2\n\nUSAGE:\n    reverts-cli generate-project-v2 --input <DB> --project-id <ID> --output <DIR>\n\nOPTIONS:\n    --input <DB>          SQLite input database\n    --project-id <ID>     Positive project id\n    --output <DIR>        Output directory for the generated TypeScript project"
        }
        HelpTopic::FullInventory => {
            "reverts-cli full-inventory\n\nUSAGE:\n    reverts-cli full-inventory --input <DB> --project-id <ID> [--manifest <FILE>] [--source-root <DIR>] [--output-root <DIR>] [--naming-progress <FILE>] [--json <FILE>]\n\nOPTIONS:\n    --input <DB>              SQLite input database\n    --project-id <ID>         Positive project id\n    --manifest <FILE>         Optional reverts-import-evidence.json for unpack/source coverage counts\n    --source-root <DIR>       Optional extracted source root for file counts\n    --output-root <DIR>       Optional generated project root for output and symbol-index counts\n    --naming-progress <FILE>  Optional naming-progress JSON to reuse instead of recomputing\n    --json <FILE>             Write JSON report to this file; without it, print JSON to stdout"
        }
        HelpTopic::CoverageLedger => {
            "reverts-cli coverage-ledger\n\nUSAGE:\n    reverts-cli coverage-ledger --input <DB> --project-id <ID> [--full-inventory <FILE>] [--manifest <FILE>] [--source-root <DIR>] [--output-root <DIR>] [--naming-progress <FILE>] [--identifier-inventory <FILE>] [--json <FILE>]\n\nOPTIONS:\n    --input <DB>                    SQLite input database\n    --project-id <ID>               Positive project id\n    --full-inventory <FILE>         Existing full-inventory JSON to use as the ledger source\n    --manifest <FILE>               Optional reverts-import-evidence.json when full inventory must be computed\n    --source-root <DIR>             Optional extracted source root when full inventory must be computed\n    --output-root <DIR>             Optional generated project root when full inventory must be computed\n    --naming-progress <FILE>        Optional naming-progress JSON when full inventory must be computed\n    --identifier-inventory <FILE>   Optional identifier-inventory JSON to fold into the unified ledger\n    --json <FILE>                   Write JSON report to this file; without it, print JSON to stdout"
        }
        HelpTopic::IdentifierInventory => {
            "reverts-cli identifier-inventory\n\nUSAGE:\n    reverts-cli identifier-inventory --output-root <DIR> [--json <FILE>]\n\nOPTIONS:\n    --output-root <DIR>   Generated project root to scan recursively for all JS/TS files; named bindings are counted only from symbol-index.json rows with semantic_named=true\n    --json <FILE>         Write JSON report to this file; without it, print JSON to stdout"
        }
        HelpTopic::MatchPackages => {
            "reverts-cli match-packages\n\nUSAGE:\n    reverts-cli match-packages --input <DB> --project-id <ID> [--package-name <NAME> ...] [--package-source-root <DIR> ...] [--materialize-package-sources] [--apply]\n\nOPTIONS:\n    --input <DB>                     SQLite input database\n    --project-id <ID>                Positive project id\n    --package-name <NAME>            Restrict matching to the package graph component containing this package; repeatable\n    --package-source-root <DIR>      Additional local package source root (package dir, node_modules, or project root containing node_modules); repeatable. Loaded files are source-only unless later proven importable.\n    --materialize-package-sources    Resolve exact/range/missing package version hints and download only concrete, compatible package versions from the npm registry into the on-disk package cache (~/.reverts/package-cache, override REVERTS_PACKAGE_CACHE_DIR) before matching; with --apply, persist collected sources to package_source_cache\n    --apply                          Persist accepted package attributions, surfaces, and materialized package source cache rows"
        }
        HelpTopic::MatchPackagesReport => {
            "reverts-cli match-packages-report\n\nUSAGE:\n    reverts-cli match-packages-report --input <DB> --all-projects [--limit <N>] [--newest] [--package-name <NAME> ...] [--package-source-root <DIR> ...] [--materialize-package-sources]\n\nOPTIONS:\n    --input <DB>                     SQLite input database\n    --all-projects                   Inspect every project id in the database\n    --limit <N>                      Maximum number of project ids to inspect\n    --newest                         Visit highest project ids first\n    --package-name <NAME>            Restrict matching to the package graph component containing this package; repeatable\n    --package-source-root <DIR>      Additional local package source root; repeatable\n    --materialize-package-sources    Resolve and download concrete package versions from the npm registry (cached on disk) for the report\n\nMETRICS:\n    direct_externalized              Package modules emitted as direct package imports\n    private_source_suppressed        Private package modules safely removed with an externalized closure\n    source_eliminated                direct_externalized + private_source_suppressed\n    source_remaining                 Package source modules still requiring source preservation"
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
        HelpTopic::PackageSurfaceDecisions => {
            "reverts-cli package-surface-decisions\n\nUSAGE:\n    reverts-cli package-surface-decisions --input <DB> --project-id <ID> [--list] [--batch <TSV>] [--apply]\n\nOPTIONS:\n    --input <DB>       SQLite input database\n    --project-id <ID>  Positive project id\n    --list             Print source-backed bare package import sites as an Agent worklist\n    --batch <TSV>      Agent decisions: OP<TAB>PACKAGE<TAB>VERSION|-<TAB>EXPORT_SPECIFIER<TAB>EVIDENCE\n    --apply            Persist decisions; accept_surface rows also write accepted package_surfaces\n\nBATCH OPS:\n    accept_surface<TAB>package<TAB>exact_version<TAB>specifier<TAB>evidence\n    reject_surface<TAB>package<TAB>version|-<TAB>specifier<TAB>evidence\n    block_surface<TAB>package<TAB>version|-<TAB>specifier<TAB>evidence"
        }
        HelpTopic::ExtractAssets => {
            "reverts-cli extract-assets\n\nUSAGE:\n    reverts-cli extract-assets --input <DB> --project-id <ID> [--asset-root <DIR-OR-BUN-EXE>]... [--apply]\n\nOPTIONS:\n    --input <DB>                    SQLite input database\n    --project-id <ID>               Positive project id\n    --asset-root <DIR-OR-BUN-EXE>   Root directory for asset files, or a Bun standalone executable for /$bunfs/root assets (repeatable)\n    --apply                         Persist discovered project_assets rows"
        }
        HelpTopic::RuntimeInventory => {
            "reverts-cli runtime-inventory\n\nUSAGE:\n    reverts-cli runtime-inventory --input <DB> (--project-id <ID> | --all-projects) [--limit <N>] [--newest] [--max-source-bytes <N>] [--setter-blockers] [--runtime-attribution] [--package-source-blockers]\n\nOPTIONS:\n    --input <DB>                   SQLite input database\n    --project-id <ID>              Positive project id to inspect\n    --all-projects                 Inspect every project id in the database\n    --limit <N>                    Maximum number of project ids to inspect when --all-projects is used\n    --newest                       Visit highest project ids first when --all-projects is used\n    --max-source-bytes <N>         Skip projects whose source_files total exceeds this byte limit\n    --setter-blockers              Also print conservative runtime setter migration blocker distribution\n    --runtime-attribution          Attribute emitted runtime helper lines by top-level binding and kind\n    --package-source-blockers      Report largest emitted package source files and why they were not eliminated"
        }
        HelpTopic::SymbolNames => {
            "reverts-cli symbol-names\n\nUSAGE:\n    reverts-cli symbol-names --input <DB> --project-id <ID> --list [--all-proposals]\n    reverts-cli symbol-names --input <DB> --project-id <ID> [--propose <MODULE_ID:ORIGINAL=SEMANTIC> ...] [--accept <MODULE_ID:ORIGINAL=SEMANTIC> ...] [--clear-active <MODULE_ID:ORIGINAL> ...] [--origin <SOURCE>] [--evidence <TEXT>] [--batch <TSV|->] [--apply]\n\nOPTIONS:\n    --input <DB>          SQLite input database\n    --project-id <ID>     Positive project id\n    --list                Print active module/global symbols as TSV\n    --all-proposals       With --list, print recorded name proposals instead of active symbols\n    --propose <SPEC>      Record a naming proposal without changing emitted output; repeatable\n    --accept <SPEC>       Record and activate a semantic name for the next emit; repeatable (--set alias)\n    --clear-active <SPEC> Clear the active semantic name; repeatable (--clear alias)\n    --origin <SOURCE>     Proposal source label, default: agent\n    --evidence <TEXT>     Optional evidence stored with proposals from this invocation\n    --batch <TSV|->       Read tab-separated propose/accept/clear-active operations from a file or stdin\n    --apply               Persist changes; without --apply, only validates and prints a dry-run summary\n\nBATCH TSV:\n    propose<TAB>module_id<TAB>original_name<TAB>semantic_name\n    accept<TAB>module_id<TAB>original_name<TAB>semantic_name\n    clear-active<TAB>module_id<TAB>original_name"
        }
        HelpTopic::NamingProgress => {
            "reverts-cli naming-progress\n\nUSAGE:\n    reverts-cli naming-progress --input <DB> --project-id <ID> [--target-level <LEVEL>] [--json]\n\nOPTIONS:\n    --input <DB>             SQLite input database (opened read-only)\n    --project-id <ID>        Positive project id\n    --target-level <LEVEL>   Headline tier: public-surface | declarations | full (default: full)\n    --json                   Print stable JSON instead of human-readable text\n\nTIERS (cumulative, first-party modules only):\n    public-surface   Exported symbols\n    declarations     + non-exported function/class declarations\n    full             + remaining module-level value/const symbols"
        }
        HelpTopic::NamingPlan => {
            "reverts-cli naming-plan\n\nUSAGE:\n    reverts-cli naming-plan --input <DB> --project-id <ID> [--target-level <LEVEL>]\n\nOPTIONS:\n    --input <DB>             SQLite input database (opened read-only)\n    --project-id <ID>        Positive project id\n    --target-level <LEVEL>   public-surface | declarations | full (default: full)\n\nEmits JSON: unnamed (minified, no semantic name) module-level bindings up to the\ntarget tier, grouped by first-party module. Join with symbol-index.json (from\ngenerate-project-v2) on (module_id, original_name) for file locations."
        }
        HelpTopic::ModuleClassify => {
            "reverts-cli module-classify\n\nUSAGE:\n    reverts-cli module-classify --input <DB> --project-id <ID> [--auto] [--batch <TSV>] [--list] [--apply]\n\nOPTIONS:\n    --input <DB>        SQLite input database\n    --project-id <ID>   Positive project id\n    --auto              Classify vendored node_modules paths as third-party (deterministic)\n    --batch <TSV>       Agent verdicts: MODULE_ID<TAB>CLASSIFICATION[<TAB>EVIDENCE]\n    --list              List recorded classifications\n    --apply             Persist (otherwise dry-run)\n\nCLASSIFICATIONS:\n    application | third-party-library | runtime-glue\n\nNOTE: classification only refines the naming denominator; it never emits a bare import. Real externalization stays with the fingerprint matcher."
        }
        HelpTopic::MatchModulesRecall => {
            "reverts-cli match-modules-recall\n\nUSAGE:\n    reverts-cli match-modules-recall --input <DB> --ground-truth-project-id <ID> --subject-project-id <ID> [--threshold-percent <N>] [--metric jaccard|overlap] [--category <NAME> ...] [--limit <N>]\n\nOPTIONS:\n    --input <DB>                      SQLite input database (opened read-only)\n    --ground-truth-project-id <ID>    Project whose semantic_names are treated as truth\n    --subject-project-id <ID>         Project whose modules are being matched\n    --threshold-percent <N>           Similarity threshold for a (ref, subject) pair to count (default 70)\n    --metric <NAME>                   jaccard (default, principled) or overlap (more forgiving subset rule)\n    --category <NAME>                 Restrict to this module_category (e.g. application, package); repeatable\n    --limit <N>                       Cap modules per project for fast iteration\n\nDIAGNOSTIC:\n    Reports recall under each available matching strategy:\n      baseline / semantic_name exact   exact equality on the existing semantic_name field\n      multi_axis_<metric>              per-axis function-fingerprint similarity (Ast, Cfg, anchors, ...) combined by max\n    Writes nothing to the database."
        }
    }
}
