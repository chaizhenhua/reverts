//! Help topics and rendered help text for the `reverts-cli` binary. Kept
//! separate from the parser/runner so that updating one piece of help copy
//! does not force a rebuild of the rest of the CLI module tree.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTopic {
    TopLevel,
    GenerateProjectV2,
    MatchPackages,
    ExtractAssets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub topic: HelpTopic,
    pub summary: &'static str,
}

pub const GENERATE_PROJECT_V2_COMMAND: &str = "generate-project-v2";
pub const MATCH_PACKAGES_COMMAND: &str = "match-packages";
pub const EXTRACT_ASSETS_COMMAND: &str = "extract-assets";

pub const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        name: MATCH_PACKAGES_COMMAND,
        topic: HelpTopic::MatchPackages,
        summary: "Populate package_attributions/package_surfaces in SQLite",
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
            "reverts-cli\n\nUSAGE:\n    reverts-cli <COMMAND> [OPTIONS]\n    reverts-cli --help [COMMAND]\n    reverts-cli --version\n\nCOMMANDS:\n    match-packages        Populate package_attributions/package_surfaces in SQLite\n    extract-assets        Populate project_assets from asset references in source slices\n    generate-project-v2   Generate a TypeScript project from SQLite input\n\nUse `reverts-cli help <COMMAND>` for command-specific help."
        }
        HelpTopic::GenerateProjectV2 => {
            "reverts-cli generate-project-v2\n\nUSAGE:\n    reverts-cli generate-project-v2 --input <DB> --project-id <ID> --output <DIR>\n\nOPTIONS:\n    --input <DB>          SQLite input database\n    --project-id <ID>     Positive project id\n    --output <DIR>        Output directory for the generated TypeScript project"
        }
        HelpTopic::MatchPackages => {
            "reverts-cli match-packages\n\nUSAGE:\n    reverts-cli match-packages --input <DB> --project-id <ID> [--package-name <NAME> ...] [--package-source-root <DIR> ...] [--materialize-package-sources] [--apply]\n\nOPTIONS:\n    --input <DB>                     SQLite input database\n    --project-id <ID>                Positive project id\n    --package-name <NAME>            Restrict matching to one package name; repeatable\n    --package-source-root <DIR>      Additional local package source root (package dir, node_modules, or project root containing node_modules); repeatable. Loaded files are source-only unless later proven importable.\n    --materialize-package-sources    Use package_name/package_version hints in the DB to npm-install exact package versions into a temporary source root before matching; with --apply, persist collected sources to package_source_cache\n    --apply                          Persist accepted package attributions, surfaces, and materialized package source cache rows"
        }
        HelpTopic::ExtractAssets => {
            "reverts-cli extract-assets\n\nUSAGE:\n    reverts-cli extract-assets --input <DB> --project-id <ID> [--asset-root <DIR-OR-BUN-EXE>]... [--apply]\n\nOPTIONS:\n    --input <DB>                    SQLite input database\n    --project-id <ID>               Positive project id\n    --asset-root <DIR-OR-BUN-EXE>   Root directory for asset files, or a Bun standalone executable for /$bunfs/root assets (repeatable)\n    --apply                         Persist discovered project_assets rows"
        }
    }
}
