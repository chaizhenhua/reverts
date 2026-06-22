//! `island-package-candidates` command: record Agent-proposed third-party
//! package names for the eager entry island.
//!
//! A scope-hoisting bundler inlines whole libraries into the island with no
//! module of their own, so the deterministic matcher has no `(name, version)`
//! to fetch. An Agent reads the island's evidence (string anchors, API shapes)
//! and proposes package names; this command records those proposals as
//! `island_package_candidates`. `match-packages` then seeds materialization
//! with the accepted names and the fingerprint cascade confirms each — a wrong
//! guess simply fails to match, so the Agent never bypasses the proof.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use rusqlite::{Connection, OpenFlags};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::errors::{CliError, CliRunError};
use crate::persistence::island_package_candidates::{
    IslandPackageCandidate, IslandPackageCandidateStatus, load_accepted_island_package_candidates,
    persist_island_package_candidate,
};

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct IslandPackageCandidatesArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    #[arg(long)]
    pub list: bool,
    #[arg(long)]
    pub apply: bool,
    /// Package name to accept as an island candidate; repeatable.
    #[arg(long = "accept")]
    pub accepts: Vec<String>,
    /// Package name to reject; repeatable.
    #[arg(long = "reject")]
    pub rejects: Vec<String>,
    /// Version specifier applied to every `--accept` on this invocation.
    #[arg(long)]
    pub version: Option<String>,
    /// Evidence string applied to every `--accept`/`--reject` on this invocation.
    #[arg(long)]
    pub evidence: Option<String>,
    /// TSV rows: `OP<TAB>PACKAGE<TAB>VERSION|-<TAB>EVIDENCE` where OP is
    /// `accept` or `reject`.
    #[arg(long)]
    pub batch: Option<PathBuf>,
}

impl IslandPackageCandidatesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == crate::help::ISLAND_PACKAGE_CANDIDATES_COMMAND)
        {
            args.remove(0);
        }
        let parsed: Self =
            parse_args_with_name(crate::help::ISLAND_PACKAGE_CANDIDATES_COMMAND, args)?;
        validate_args(parsed)
    }
}

pub(crate) fn validate_args(
    args: IslandPackageCandidatesArgs,
) -> Result<IslandPackageCandidatesArgs, CliError> {
    let mutating = !args.accepts.is_empty() || !args.rejects.is_empty() || args.batch.is_some();
    if args.list && (mutating || args.apply) {
        return Err(CliError::UnknownArgument(
            "--list cannot be combined with mutations".to_string(),
        ));
    }
    if !args.list && !mutating {
        return Err(CliError::MissingArgument(
            "--list | --accept | --reject | --batch",
        ));
    }
    Ok(args)
}

/// One proposal to persist, resolved from CLI flags or a batch row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Proposal {
    candidate: IslandPackageCandidate,
    status: IslandPackageCandidateStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IslandPackageCandidatesOutcome {
    pub listed: Vec<(String, Option<String>, String)>,
    pub requested_changes: usize,
    pub written_changes: usize,
}

pub(crate) fn run(args: IslandPackageCandidatesArgs) -> Result<(), CliRunError> {
    let outcome = island_package_candidates_from_sqlite(&args)?;
    if args.list {
        println!("package_name\tversion_hint\tevidence");
        for (name, version, evidence) in outcome.listed {
            println!("{name}\t{}\t{evidence}", version.unwrap_or_default());
        }
    } else if args.apply {
        println!(
            "updated island package candidates for project {}: {} change(s) written",
            args.project_id, outcome.written_changes
        );
    } else {
        println!(
            "dry-run: would record {} island package candidate(s) for project {}; pass --apply to persist",
            outcome.requested_changes, args.project_id
        );
    }
    Ok(())
}

pub fn island_package_candidates_from_sqlite(
    args: &IslandPackageCandidatesArgs,
) -> Result<IslandPackageCandidatesOutcome, CliRunError> {
    let flags = if args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection = Connection::open_with_flags(args.input.as_path(), flags)
        .map_err(|source| CliRunError::IslandPackageCandidates(source.to_string()))?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(|source| CliRunError::IslandPackageCandidates(source.to_string()))?;

    if args.list {
        let listed =
            load_accepted_island_package_candidates(&connection, i64::from(args.project_id))
                .map_err(|source| CliRunError::IslandPackageCandidates(source.to_string()))?
                .into_iter()
                .map(|candidate| {
                    (
                        candidate.package_name,
                        candidate.version_hint,
                        candidate.evidence,
                    )
                })
                .collect();
        return Ok(IslandPackageCandidatesOutcome {
            listed,
            requested_changes: 0,
            written_changes: 0,
        });
    }

    let proposals = collect_proposals(args)?;

    let written_changes = if args.apply {
        let mut written = 0_usize;
        for proposal in &proposals {
            persist_island_package_candidate(
                &mut connection,
                i64::from(args.project_id),
                &proposal.candidate,
                proposal.status,
            )
            .map_err(|source| CliRunError::IslandPackageCandidates(source.to_string()))?;
            written += 1;
        }
        written
    } else {
        0
    };

    Ok(IslandPackageCandidatesOutcome {
        listed: Vec::new(),
        requested_changes: proposals.len(),
        written_changes,
    })
}

fn collect_proposals(args: &IslandPackageCandidatesArgs) -> Result<Vec<Proposal>, CliRunError> {
    let mut proposals = Vec::new();
    for name in &args.accepts {
        proposals.push(Proposal {
            candidate: IslandPackageCandidate {
                package_name: name.trim().to_string(),
                version_hint: args.version.clone(),
                evidence: args.evidence.clone().unwrap_or_default(),
            },
            status: IslandPackageCandidateStatus::Accepted,
        });
    }
    for name in &args.rejects {
        proposals.push(Proposal {
            candidate: IslandPackageCandidate {
                package_name: name.trim().to_string(),
                version_hint: None,
                evidence: args.evidence.clone().unwrap_or_default(),
            },
            status: IslandPackageCandidateStatus::Rejected,
        });
    }
    if let Some(batch) = &args.batch {
        let contents = fs::read_to_string(batch)
            .map_err(|source| CliRunError::IslandPackageCandidates(source.to_string()))?;
        for proposal in parse_batch(contents.as_str())? {
            proposals.push(proposal);
        }
    }
    for proposal in &proposals {
        if proposal.candidate.package_name.is_empty() {
            return Err(CliRunError::IslandPackageCandidates(
                "package name must not be empty".to_string(),
            ));
        }
    }
    Ok(proposals)
}

/// Parse `OP<TAB>PACKAGE<TAB>VERSION|-<TAB>EVIDENCE` rows. Blank lines skipped.
fn parse_batch(contents: &str) -> Result<Vec<Proposal>, CliRunError> {
    let mut proposals = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 2 {
            return Err(CliRunError::IslandPackageCandidates(format!(
                "batch row needs at least OP<TAB>PACKAGE: {line:?}"
            )));
        }
        let status = match fields[0].trim() {
            "accept" => IslandPackageCandidateStatus::Accepted,
            "reject" => IslandPackageCandidateStatus::Rejected,
            other => {
                return Err(CliRunError::IslandPackageCandidates(format!(
                    "batch op must be accept|reject, got {other:?}"
                )));
            }
        };
        let version_hint = fields
            .get(2)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty() && *value != "-")
            .map(str::to_string);
        let evidence = fields.get(3).map(|value| value.trim()).unwrap_or_default();
        proposals.push(Proposal {
            candidate: IslandPackageCandidate {
                package_name: fields[1].trim().to_string(),
                version_hint,
                evidence: evidence.to_string(),
            },
            status,
        });
    }
    Ok(proposals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_batch_reads_accept_and_reject_rows() {
        let proposals = parse_batch(
            "accept\tzod\t^3.25.64\tZodError anchors\n\
             reject\tleft-pad\t-\tweak\n\
             \n",
        )
        .expect("parse batch");
        assert_eq!(proposals.len(), 2);
        assert_eq!(proposals[0].candidate.package_name, "zod");
        assert_eq!(
            proposals[0].candidate.version_hint.as_deref(),
            Some("^3.25.64")
        );
        assert_eq!(proposals[0].status, IslandPackageCandidateStatus::Accepted);
        assert_eq!(proposals[1].candidate.package_name, "left-pad");
        assert!(proposals[1].candidate.version_hint.is_none());
        assert_eq!(proposals[1].status, IslandPackageCandidateStatus::Rejected);
    }

    #[test]
    fn validate_args_requires_an_action() {
        let args = IslandPackageCandidatesArgs {
            input: PathBuf::from("p.sqlite"),
            project_id: 1,
            list: false,
            apply: false,
            accepts: Vec::new(),
            rejects: Vec::new(),
            version: None,
            evidence: None,
            batch: None,
        };
        assert!(validate_args(args).is_err());
    }
}
