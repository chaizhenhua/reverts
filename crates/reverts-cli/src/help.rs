//! Help topics and rendered help text for the `reverts-cli` binary. Kept
//! separate from the parser/runner so that updating one piece of help copy
//! does not force a rebuild of the rest of the CLI module tree.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTopic {
    TopLevel,
    NameGroup,
    PackageGroup,
    ReportGroup,
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
    BindingNames,
    ReferenceSourceNames,
    OwnershipSourceNames,
    SymbolNames,
    NamingProgress,
    NamingPlan,
    ModuleClassify,
    ModuleNames,
    ClusterNames,
    IslandPackageCandidates,
    MatchModulesRecall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub topic: HelpTopic,
    pub summary: &'static str,
}

pub const GENERATE_COMMAND: &str = "generate";
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
pub const BINDING_NAMES_COMMAND: &str = "binding-names";
pub const REFERENCE_SOURCE_NAMES_COMMAND: &str = "reference-source-names";
pub const OWNERSHIP_SOURCE_NAMES_COMMAND: &str = "ownership-source-names";
pub const SYMBOL_NAMES_COMMAND: &str = "symbol-names";
pub const NAMING_PROGRESS_COMMAND: &str = "naming-progress";
pub const NAMING_PLAN_COMMAND: &str = "naming-plan";
pub const MODULE_CLASSIFY_COMMAND: &str = "module-classify";
pub const MODULE_NAMES_COMMAND: &str = "module-names";
pub const CLUSTER_NAMES_COMMAND: &str = "cluster-names";
pub const ISLAND_PACKAGE_CANDIDATES_COMMAND: &str = "island-package-candidates";
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
        name: GENERATE_COMMAND,
        topic: HelpTopic::GenerateProjectV2,
        summary: "Generate a TypeScript project from SQLite input",
    },
    CommandSpec {
        name: RUNTIME_INVENTORY_COMMAND,
        topic: HelpTopic::RuntimeInventory,
        summary: "Measure emitted runtime helpers and generated internal names",
    },
    CommandSpec {
        name: BINDING_NAMES_COMMAND,
        topic: HelpTopic::BindingNames,
        summary: "Accept generated-output local binding semantic names",
    },
    CommandSpec {
        name: REFERENCE_SOURCE_NAMES_COMMAND,
        topic: HelpTopic::ReferenceSourceNames,
        summary: "Name modules/exports/bindings from a historical first-party source tree",
    },
    CommandSpec {
        name: OWNERSHIP_SOURCE_NAMES_COMMAND,
        topic: HelpTopic::OwnershipSourceNames,
        summary: "Name owned-but-inlined package modules from their matched package source",
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
        name: MODULE_NAMES_COMMAND,
        topic: HelpTopic::ModuleNames,
        summary: "Accept semantic file paths for first-party modules (renames emitted files)",
    },
    CommandSpec {
        name: CLUSTER_NAMES_COMMAND,
        topic: HelpTopic::ClusterNames,
        summary: "Accept semantic file paths for island clusters by content fingerprint",
    },
    CommandSpec {
        name: ISLAND_PACKAGE_CANDIDATES_COMMAND,
        topic: HelpTopic::IslandPackageCandidates,
        summary: "Record Agent-proposed third-party package names for the eager entry island",
    },
    CommandSpec {
        name: MATCH_MODULES_RECALL_COMMAND,
        topic: HelpTopic::MatchModulesRecall,
        summary: "Measure cross-project module match recall against a ground-truth project",
    },
];

#[must_use]
pub fn command_topic(command: &str) -> Option<HelpTopic> {
    // `generate-project-v2` is the deprecated alias for `generate`.
    let command = if command == "generate-project-v2" {
        GENERATE_COMMAND
    } else {
        command
    };
    match command {
        "name" => return Some(HelpTopic::NameGroup),
        "package" => return Some(HelpTopic::PackageGroup),
        "report" => return Some(HelpTopic::ReportGroup),
        _ => {}
    }
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
            "reverts-cli\n\nUSAGE:\n    reverts-cli <COMMAND> [OPTIONS]\n    reverts-cli <GROUP> <SUBCOMMAND> [OPTIONS]\n    reverts-cli --help [COMMAND]\n    reverts-cli --version\n\nPIPELINE:\n    import       Import unpack evidence into Reverts SQLite facts\n    match        Populate package attributions and surfaces\n    classify     Classify modules to refine the naming denominator\n    name         Assign semantic names (see `reverts-cli help name`)\n    generate     Generate a TypeScript project from SQLite input\n\nGROUPS:\n    name <subject>     symbols | bindings | modules | clusters | plan | progress | from-reference | from-package\n    package <command>  candidates | hints | surface | versions | cache {audit,prune}\n    report <what>      coverage | inventory | identifiers | runtime | packages\n\nOTHER:\n    assets extract     Populate project_assets from asset references in source slices\n    dev recall         Measure cross-project module match recall (evaluation)\n\nUse `reverts-cli help <COMMAND>` for command-specific help.\nLegacy flat names (symbol-names, generate-project-v2, ...) remain accepted as aliases."
        }
        HelpTopic::NameGroup => {
            "reverts-cli name <subject>\n\nUSAGE:\n    reverts-cli name <subject> [OPTIONS]\n\nAssign semantic names. Dry-run by default; pass --apply to persist.\n\nSUBJECTS:\n    symbols          List/propose/accept symbol semantic names      (alias: symbol-names)\n    bindings         Accept generated-output local binding names     (alias: binding-names)\n    modules          Accept semantic file paths for first-party modules (alias: module-names)\n    clusters         Accept semantic file paths for island clusters  (alias: cluster-names)\n    plan             Emit the JSON work list of unnamed symbols      (alias: naming-plan)\n    progress         Report semantic-naming completion               (alias: naming-progress)\n    from-reference   Name from a historical first-party source tree  (alias: reference-source-names)\n    from-package     Name owned-but-inlined package modules from source (alias: ownership-source-names)\n\nUse `reverts-cli name <subject> --help` for subject-specific options."
        }
        HelpTopic::PackageGroup => {
            "reverts-cli package <command>\n\nUSAGE:\n    reverts-cli package <command> [OPTIONS]\n\nCOMMANDS:\n    candidates       Record Agent-proposed island package names      (alias: island-package-candidates)\n    hints            Generate verified externalization hint rows      (alias: package-externalization-hints)\n    surface          Apply Agent-resolved package surface decisions   (alias: package-surface-decisions)\n    versions         Diagnose rejected package-version matches        (alias: package-version-diagnostics)\n    cache audit      Audit package_source_cache freshness and validity (alias: package-cache-audit)\n    cache prune      Delete invalid/stale package_source_cache rows   (alias: package-cache-prune-stale)\n\nUse `reverts-cli package <command> --help` for command-specific options."
        }
        HelpTopic::ReportGroup => {
            "reverts-cli report <what>\n\nUSAGE:\n    reverts-cli report <what> [OPTIONS]\n\nREPORTS:\n    coverage         Unified decompile coverage ledger               (alias: coverage-ledger)\n    inventory        Full decompile inventory and coverage report    (alias: full-inventory)\n    identifiers      Count AST identifier sites in generated output   (alias: identifier-inventory)\n    runtime          Measure emitted runtime helpers and internal names (alias: runtime-inventory)\n    packages         Package match/externalization/source rates       (alias: match-packages-report)\n\nUse `reverts-cli report <what> --help` for report-specific options."
        }
        HelpTopic::ImportUnpacked => {
            "reverts-cli import-unpacked\n\nUSAGE:\n    reverts-cli import-unpacked --input <UNPACKED_ROOT> --manifest <MANIFEST> --project-name <NAME> --output-db <DB> [--ignore-native-assets] [--max-source-bytes <N>] [--bundle-source-bytes <N>]\n\nOPTIONS:\n    --input <UNPACKED_ROOT>       Unpacked source root, for Electron usually Contents/Resources/app\n    --manifest <MANIFEST>         Authoritative reverts.import_evidence.v1 manifest; every input file must be covered and recorded size/hash evidence must match\n    --project-name <NAME>         Project name stored in Reverts SQLite\n    --output-db <DB>              SQLite database to create\n    --ignore-native-assets        Do not write native assets into project_assets after manifest validation\n    --max-source-bytes <N>        Defer source files larger than N bytes as project_assets instead of modules\n    --bundle-source-bytes <N>     Keep source files larger than N bytes as source_files without module rows so the pipeline can extract bundled modules\n\nOUTPUT:\n    Creates canonical Reverts facts: projects, source_files (with file_size), project_files, modules, module_dependencies, project_assets, and package_attributions."
        }
        HelpTopic::GenerateProjectV2 => {
            "reverts-cli generate\n\nUSAGE:\n    reverts-cli generate --input <DB> --project-id <ID> --output <DIR> [--source-root <DIR>]\n\nOPTIONS:\n    --input <DB>          SQLite input database\n    --project-id <ID>     Positive project id\n    --output <DIR>        Output directory for the generated TypeScript project\n    --source-root <DIR>   Emit recovered source under this directory (e.g. `src`) for a modern\n                          TypeScript layout: NodeNext resolution, package.json `exports`,\n                          .gitignore, and pipeline metadata moved to `.reverts/`.\n                          Omit for the flat legacy layout."
        }
        HelpTopic::FullInventory => {
            "reverts-cli full-inventory\n\nUSAGE:\n    reverts-cli full-inventory --input <DB> --project-id <ID> [--manifest <FILE>] [--source-root <DIR>] [--output-root <DIR>] [--naming-progress <FILE>] [--json <FILE>]\n\nOPTIONS:\n    --input <DB>              SQLite input database\n    --project-id <ID>         Positive project id\n    --manifest <FILE>         Optional reverts-import-evidence.json for unpack/source coverage counts\n    --source-root <DIR>       Optional extracted source root for file counts\n    --output-root <DIR>       Optional generated project root for output and symbol-index counts\n    --naming-progress <FILE>  Optional naming-progress JSON to reuse instead of recomputing\n    --json <FILE>             Write JSON report to this file; without it, print JSON to stdout"
        }
        HelpTopic::CoverageLedger => {
            "reverts-cli coverage-ledger\n\nUSAGE:\n    reverts-cli coverage-ledger --input <DB> --project-id <ID> [--full-inventory <FILE>] [--manifest <FILE>] [--source-root <DIR>] [--output-root <DIR>] [--naming-progress <FILE>] [--identifier-inventory <FILE>] [--json <FILE>]\n\nOPTIONS:\n    --input <DB>                    SQLite input database\n    --project-id <ID>               Positive project id\n    --full-inventory <FILE>         Existing full-inventory JSON to use as the ledger source\n    --manifest <FILE>               Optional reverts-import-evidence.json when full inventory must be computed\n    --source-root <DIR>             Optional extracted source root when full inventory must be computed\n    --output-root <DIR>             Optional generated project root when full inventory must be computed\n    --naming-progress <FILE>        Optional naming-progress JSON when full inventory must be computed\n    --identifier-inventory <FILE>   Optional identifier-inventory JSON to fold into the unified ledger\n    --json <FILE>                   Write JSON report to this file; without it, print JSON to stdout"
        }
        HelpTopic::IdentifierInventory => {
            "reverts-cli identifier-inventory\n\nUSAGE:\n    reverts-cli identifier-inventory --output-root <DIR> [--json <FILE>]\n\nOPTIONS:\n    --output-root <DIR>   Generated project root to scan recursively for all JS/TS files; named bindings are counted only from symbol-index.json or binding-name-index.json rows with semantic_named=true\n    --json <FILE>         Write JSON report to this file; without it, print JSON to stdout"
        }
        HelpTopic::MatchPackages => {
            "reverts-cli match-packages\n\nUSAGE:\n    reverts-cli match-packages --input <DB> --project-id <ID> [--package-name <NAME> ...] [--package-source-root <DIR> ...] [--reference-source-root <DIR> ...] [--materialize-package-sources] [--apply]\n\nOPTIONS:\n    --input <DB>                     SQLite input database\n    --project-id <ID>                Positive project id\n    --package-name <NAME>            Restrict matching to the package graph component containing this package; repeatable\n    --package-source-root <DIR>      Additional local package source root (package dir, node_modules, or project root containing node_modules); repeatable. Loaded files are source-only unless later proven importable.\n    --reference-source-root <DIR>    Reference first-party source root to scan with OXC for bare package imports and package.json dependencies; repeatable\n    --materialize-package-sources    Resolve exact/range/missing package version hints and download only concrete, compatible package versions from the npm registry into the on-disk package cache (~/.reverts/package-cache, override REVERTS_PACKAGE_CACHE_DIR) before matching; with --apply, persist collected sources to package_source_cache\n    --apply                          Persist accepted package attributions, surfaces, and materialized package source cache rows"
        }
        HelpTopic::MatchPackagesReport => {
            "reverts-cli match-packages-report\n\nUSAGE:\n    reverts-cli match-packages-report --input <DB> --all-projects [--limit <N>] [--newest] [--package-name <NAME> ...] [--package-source-root <DIR> ...] [--reference-source-root <DIR> ...] [--materialize-package-sources]\n\nOPTIONS:\n    --input <DB>                     SQLite input database\n    --all-projects                   Inspect every project id in the database\n    --limit <N>                      Maximum number of project ids to inspect\n    --newest                         Visit highest project ids first\n    --package-name <NAME>            Restrict matching to the package graph component containing this package; repeatable\n    --package-source-root <DIR>      Additional local package source root; repeatable\n    --reference-source-root <DIR>    Reference first-party source root to scan for package candidates; repeatable\n    --materialize-package-sources    Resolve and download concrete package versions from the npm registry (cached on disk) for the report\n\nMETRICS:\n    direct_externalized              Package modules emitted as direct package imports\n    private_source_suppressed        Private package modules safely removed with an externalized closure\n    source_eliminated                direct_externalized + private_source_suppressed\n    source_remaining                 Package source modules still requiring source preservation"
        }
        HelpTopic::PackageVersionDiagnostics => {
            "reverts-cli package-version-diagnostics\n\nUSAGE:\n    reverts-cli package-version-diagnostics --input <DB> --project-id <ID> [--package-name <NAME> ...] [--package-source-root <DIR> ...] [--reference-source-root <DIR> ...] [--materialize-package-sources] [--top <N>]\n\nOPTIONS:\n    --input <DB>                     SQLite input database (opened read-only)\n    --project-id <ID>                Positive project id\n    --package-name <NAME>            Restrict diagnostics to one package; repeatable\n    --package-source-root <DIR>      Additional local package source root; repeatable\n    --reference-source-root <DIR>    Reference first-party source root to scan for package candidates; repeatable\n    --materialize-package-sources    Resolve and npm-install candidate package sources in-memory only; never writes SQLite\n    --top <N>                        Candidate versions to print per package (default: 5)\n\nDIAGNOSTIC:\n    Inspects rejected package_attributions whose reason is selected package version did not match this module source, treats DB versions as hints, and scores cached/source-root candidate versions against module source fingerprints without applying changes."
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
            "reverts-cli package-surface-decisions\n\nUSAGE:\n    reverts-cli package-surface-decisions --input <DB> --project-id <ID> [--list] [--batch <TSV>] [--apply] [--replace-existing]\n\nOPTIONS:\n    --input <DB>             SQLite input database\n    --project-id <ID>        Positive project id\n    --list                   Print source-backed bare package import sites as an Agent worklist, including candidate_versions and latest Agent decision\n    --batch <TSV>            Agent decisions: OP<TAB>PACKAGE<TAB>VERSION|-<TAB>EXPORT_SPECIFIER<TAB>EVIDENCE\n    --apply                  Persist decisions; accept_surface rows also write accepted package_surfaces\n    --replace-existing       Allow an Agent-resolved accept_surface to replace a conflicting accepted package_surface; requires --apply\n\nBATCH OPS:\n    accept_surface<TAB>package<TAB>exact_version<TAB>specifier<TAB>evidence\n    reject_surface<TAB>package<TAB>version|-<TAB>specifier<TAB>evidence\n    block_surface<TAB>package<TAB>version|-<TAB>specifier<TAB>evidence\n\nMATCH-PACKAGES GATE:\n    The latest reject_surface or block_surface for a specifier suppresses future match-packages surface acceptance until a later accept_surface is applied."
        }
        HelpTopic::ExtractAssets => {
            "reverts-cli extract-assets\n\nUSAGE:\n    reverts-cli extract-assets --input <DB> --project-id <ID> [--asset-root <DIR-OR-BUN-EXE>]... [--apply]\n\nOPTIONS:\n    --input <DB>                    SQLite input database\n    --project-id <ID>               Positive project id\n    --asset-root <DIR-OR-BUN-EXE>   Root directory for asset files, or a Bun standalone executable for /$bunfs/root assets (repeatable)\n    --apply                         Persist discovered project_assets rows"
        }
        HelpTopic::RuntimeInventory => {
            "reverts-cli runtime-inventory\n\nUSAGE:\n    reverts-cli runtime-inventory --input <DB> (--project-id <ID> | --all-projects) [--limit <N>] [--newest] [--max-source-bytes <N>] [--setter-blockers] [--runtime-attribution] [--package-source-blockers] [--init-cycles]\n\nOPTIONS:\n    --input <DB>                   SQLite input database\n    --project-id <ID>              Positive project id to inspect\n    --all-projects                 Inspect every project id in the database\n    --limit <N>                    Maximum number of project ids to inspect when --all-projects is used\n    --newest                       Visit highest project ids first when --all-projects is used\n    --max-source-bytes <N>         Skip projects whose source_files total exceeds this byte limit\n    --setter-blockers              Also print conservative runtime setter migration blocker distribution\n    --runtime-attribution          Attribute emitted runtime helper lines by top-level binding and kind\n    --package-source-blockers      Report largest emitted package source files and why they were not eliminated\n    --init-cycles                  Report module init-dependency cycle structure (import vs init-time vs read-core SCCs)"
        }
        HelpTopic::BindingNames => {
            "reverts-cli binding-names\n\nUSAGE:\n    reverts-cli binding-names --input <DB> --project-id <ID> (--list | --accept <FILE:ORIGINAL=SEMANTIC>... | --batch <TSV>) [--origin <SOURCE>] [--evidence <TEXT>] [--apply]\n\nOPTIONS:\n    --input <DB>           SQLite input database\n    --project-id <ID>      Project id to update\n    --list                 List accepted generated-output binding names\n    --accept <SPEC>        Accept one binding name; format FILE_PATH:ORIGINAL_NAME=SEMANTIC_NAME or FILE_PATH:ORIGINAL_NAME#BINDING_INDEX=SEMANTIC_NAME\n    --batch <TSV>          TSV rows: accept<TAB>FILE_PATH<TAB>ORIGINAL_NAME<TAB>SEMANTIC_NAME<TAB>[EVIDENCE] or accept<TAB>FILE_PATH<TAB>ORIGINAL_NAME<TAB>BINDING_INDEX<TAB>SEMANTIC_NAME<TAB>[EVIDENCE]\n    --origin <SOURCE>      Name source label, default: agent\n    --evidence <TEXT>      Evidence for names from automated origins; required for origin=agent/llm/model/etc.\n    --apply                Persist changes; without it, dry-run validation only"
        }
        HelpTopic::ReferenceSourceNames => {
            "reverts-cli reference-source-names\n\nUSAGE:\n    reverts-cli reference-source-names --input <DB> --project-id <ID> --reference-source-root <DIR> --reference-version <VERSION> [--apply] [--min-tier high|medium] [--origin-prefix source] [--module-only] [--summary-json <FILE>] [--diagnostics-json <FILE>]\n\nOPTIONS:\n    --input <DB>                    SQLite input database\n    --project-id <ID>               Positive project id\n    --reference-source-root <DIR>   Root of the historical first-party source tree to match against\n    --reference-version <VERSION>   Version label stored with accepted names (e.g. 2.1.76)\n    --apply                         Persist accepted names; without it, dry-run only\n    --min-tier high|medium          Minimum match tier to auto-accept (default: high)\n    --origin-prefix source          Prefix stored in the origin field of accepted names (default: source)\n    --module-only                   Fast module-name matching from bundle-extracted input; skips path overrides and generated-output binding propagation\n    --summary-json <FILE>           Write a machine-readable module match summary
    --diagnostics-json <FILE>       Write Low/Medium boundary diagnostics for matcher tuning"
        }
        HelpTopic::OwnershipSourceNames => {
            "reverts-cli ownership-source-names\n\nUSAGE:\n    reverts-cli ownership-source-names --input <DB> --project-id <ID> [--cache-db <DB>] [--origin-prefix package-source] [--apply]\n\nOPTIONS:\n    --input <DB>                 Per-run SQLite input database (holds package_attributions)\n    --project-id <ID>            Positive project id\n    --cache-db <DB>              Package source cache database (default: $HOME/.reverts/.reverts.db)\n    --origin-prefix <PREFIX>     Non-automated origin prefix stored with accepted names (default: package-source)\n    --apply                      Persist recovered names; without it, dry-run only\n\nNames the functions of package-owned modules that could not be safely\nexternalized (status=rejected ownership matches) by matching their bundle\nfunctions against the matched npm package source from the cache. Independent of\nexternalization: inlined package modules still get real function names."
        }
        HelpTopic::SymbolNames => {
            "reverts-cli symbol-names\n\nUSAGE:\n    reverts-cli symbol-names --input <DB> --project-id <ID> --list [--all-proposals]\n    reverts-cli symbol-names --input <DB> --project-id <ID> [--propose <MODULE_ID:ORIGINAL=SEMANTIC> ...] [--accept <MODULE_ID:ORIGINAL=SEMANTIC> ...] [--clear-active <MODULE_ID:ORIGINAL> ...] [--origin <SOURCE>] [--evidence <TEXT>] [--batch <TSV|->] [--apply]\n\nOPTIONS:\n    --input <DB>          SQLite input database\n    --project-id <ID>     Positive project id\n    --list                Print active module/global symbols as TSV\n    --all-proposals       With --list, print recorded name proposals instead of active symbols\n    --propose <SPEC>      Record a naming proposal without changing emitted output; repeatable\n    --accept <SPEC>       Record and activate a semantic name for the next emit; repeatable\n    --clear-active <SPEC> Clear the active semantic name; repeatable\n    --origin <SOURCE>     Proposal source label, default: agent\n    --evidence <TEXT>     Evidence for names from automated origins; required for origin=agent/llm/model/etc.\n    --batch <TSV|->       Read tab-separated propose/accept/clear-active operations from a file or stdin\n    --apply               Persist changes; without --apply, only validates and prints a dry-run summary\n\nBATCH TSV (op<TAB>key<TAB>original<TAB>semantic<TAB>[evidence] — same shape as binding-names):\n    propose<TAB>module_id<TAB>original_name<TAB>semantic_name<TAB>[evidence]\n    accept<TAB>module_id<TAB>original_name<TAB>semantic_name<TAB>[evidence]\n    clear-active<TAB>module_id<TAB>original_name"
        }
        HelpTopic::NamingProgress => {
            "reverts-cli naming-progress\n\nUSAGE:\n    reverts-cli naming-progress --input <DB> --project-id <ID> [--target-level <LEVEL>] [--json] [--symbol-index <JSON>]\n\nOPTIONS:\n    --input <DB>             SQLite input database (opened read-only)\n    --project-id <ID>        Positive project id\n    --target-level <LEVEL>   Headline tier: public-surface | declarations | full (default: full)\n    --json                   Print stable JSON instead of human-readable text\n    --symbol-index <JSON>    Reuse generate symbol-index.json instead of re-emitting\n\nTIERS (cumulative, first-party modules only):\n    public-surface   Exported symbols\n    declarations     + non-exported function/class declarations\n    full             + remaining module-level value/const symbols"
        }
        HelpTopic::NamingPlan => {
            "reverts-cli naming-plan\n\nUSAGE:\n    reverts-cli naming-plan --input <DB> --project-id <ID> [--target-level <LEVEL>] [--symbol-index <JSON>]\n\nOPTIONS:\n    --input <DB>             SQLite input database (opened read-only)\n    --project-id <ID>        Positive project id\n    --target-level <LEVEL>   public-surface | declarations | full (default: full)\n    --symbol-index <JSON>    Reuse generate symbol-index.json instead of re-emitting\n\nEmits JSON: unnamed (minified, no semantic name) module-level bindings up to the\ntarget tier, grouped by first-party module. Use --symbol-index with the just\nemitted output/symbol-index.json in automated decompile loops."
        }
        HelpTopic::ModuleClassify => {
            "reverts-cli module-classify\n\nUSAGE:\n    reverts-cli module-classify --input <DB> --project-id <ID> [--auto] [--batch <TSV>] [--list] [--apply]\n\nOPTIONS:\n    --input <DB>        SQLite input database\n    --project-id <ID>   Positive project id\n    --auto              Classify vendored node_modules paths as third-party (deterministic)\n    --batch <TSV>       Agent verdicts: MODULE_ID<TAB>CLASSIFICATION[<TAB>EVIDENCE]\n    --list              List recorded classifications\n    --apply             Persist (otherwise dry-run)\n\nCLASSIFICATIONS:\n    application | third-party-library | runtime-glue\n\nNOTE: classification only refines the naming denominator; it never emits a bare import. Real externalization stays with the fingerprint matcher."
        }
        HelpTopic::ModuleNames => {
            "reverts-cli module-names\n\nUSAGE:\n    reverts-cli module-names --input <DB> --project-id <ID> (--list | --accept <MODULE_ID=SEMANTIC_PATH>... | --batch <TSV>) [--origin <SOURCE>] [--evidence <TEXT>] [--apply]\n\nOPTIONS:\n    --input <DB>        SQLite input database\n    --project-id <ID>   Positive project id\n    --list              List accepted module path overrides\n    --accept <SPEC>     Accept one module file path; format MODULE_ID=SEMANTIC_PATH (e.g. 247=feature/markdown-renderer)\n    --batch <TSV>       TSV rows: accept<TAB>MODULE_ID<TAB>SEMANTIC_PATH<TAB>[EVIDENCE]\n    --origin <SOURCE>   Name source label, default: agent\n    --evidence <TEXT>   Evidence for paths from automated origins\n    --apply             Persist changes; without it, dry-run validation only\n\nAccepted paths are stored as module_path_overrides and consumed by\ngenerate: each module's emitted file moves to the semantic path and\nevery importing file's relative specifier is recomputed. The wire/export names\nare untouched, so the build still links."
        }
        HelpTopic::ClusterNames => {
            "reverts-cli cluster-names\n\nUSAGE:\n    reverts-cli cluster-names --input <DB> --project-id <ID> (--list | --accept <FINGERPRINT=SEMANTIC_PATH>... | --batch <TSV>) [--origin <SOURCE>] [--evidence <TEXT>] [--apply]\n\nOPTIONS:\n    --input <DB>        SQLite input database\n    --project-id <ID>   Positive project id\n    --list              List accepted island-cluster name overrides\n    --accept <SPEC>     Accept one island file path; format FINGERPRINT=SEMANTIC_PATH (e.g. 3066d34e2f3b70cb=telemetry/opentelemetry-instrumentation)\n    --batch <TSV>       TSV rows: accept<TAB>FINGERPRINT<TAB>SEMANTIC_PATH<TAB>[EVIDENCE]\n    --origin <SOURCE>   Name source label, default: agent\n    --evidence <TEXT>   Evidence for paths from automated origins\n    --apply             Persist changes; without it, dry-run validation only\n\nIsland clusters (Louvain communities / chain-split chunks drained out of the\neager entry) emit at mechanical modules/island/cluster-<id>.ts paths. The\nFINGERPRINT is the cluster's stable content digest printed in\n.reverts/island-clusters.json by generate; it survives the rename, so\na name keeps applying across regenerations. Accepted rows are stored as\nisland_cluster_names and consumed by generate: the cluster's emitted\nfile moves UNDER modules/island/<SEMANTIC_PATH>.ts and every importer's relative\nspecifier is recomputed. The SEMANTIC_PATH is relative to modules/island/."
        }
        HelpTopic::IslandPackageCandidates => {
            "reverts-cli island-package-candidates\n\nUSAGE:\n    reverts-cli island-package-candidates --input <DB> --project-id <ID> (--list | --accept <PACKAGE>... [--version <V>] | --reject <PACKAGE>... | --batch <TSV>) [--evidence <TEXT>] [--apply]\n\nOPTIONS:\n    --input <DB>        SQLite input database\n    --project-id <ID>   Positive project id\n    --list              List accepted island package candidates\n    --accept <PACKAGE>  Accept a proposed library name inlined in the entry island; repeatable\n    --reject <PACKAGE>  Reject a previously proposed name; repeatable\n    --version <V>       Version specifier applied to every --accept (else the matcher resolves latest)\n    --evidence <TEXT>   Evidence for the proposal (string anchors / API shapes seen in the island)\n    --batch <TSV>       TSV rows: OP<TAB>PACKAGE<TAB>VERSION|-<TAB>EVIDENCE (OP = accept|reject)\n    --apply             Persist changes; without it, dry-run validation only\n\nA scope-hoisting bundler inlines whole libraries into the eager island with no\nmodule of their own, so the deterministic matcher has no (name, version) to\nfetch. An Agent reads the island and proposes package names; match-packages\nseeds materialization with the accepted names and the fingerprint cascade\nconfirms each. A wrong guess simply fails to match and produces no anchor, so\nthe Agent's judgement never bypasses the deterministic proof."
        }
        HelpTopic::MatchModulesRecall => {
            "reverts-cli match-modules-recall\n\nUSAGE:\n    reverts-cli match-modules-recall --input <DB> --ground-truth-project-id <ID> --subject-project-id <ID> [--threshold-percent <N>] [--metric jaccard|overlap] [--category <NAME> ...] [--limit <N>]\n\nOPTIONS:\n    --input <DB>                      SQLite input database (opened read-only)\n    --ground-truth-project-id <ID>    Project whose semantic_names are treated as truth\n    --subject-project-id <ID>         Project whose modules are being matched\n    --threshold-percent <N>           Similarity threshold for a (ref, subject) pair to count (default 70)\n    --metric <NAME>                   jaccard (default, principled) or overlap (more forgiving subset rule)\n    --category <NAME>                 Restrict to this module_category (e.g. application, package); repeatable\n    --limit <N>                       Cap modules per project for fast iteration\n\nDIAGNOSTIC:\n    Reports recall under each available matching strategy:\n      baseline / semantic_name exact   exact equality on the existing semantic_name field\n      multi_axis_<metric>              per-axis function-fingerprint similarity (Ast, Cfg, anchors, ...) combined by max\n    Writes nothing to the database."
        }
    }
}
