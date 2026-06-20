//! Deterministic gates for accepting externally proposed human-readable names.
//!
//! These checks intentionally cover only facts the CLI can verify locally:
//! identifier validity, provenance presence for automated origins, and whether
//! automated names introduce tokens that are absent from their recorded evidence
//! and from the small built-in technical/role vocabulary. They do not attempt to
//! prove semantic correctness.

use std::collections::BTreeSet;

use reverts_js::{is_generated_placeholder_identifier, sanitize_identifier};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NamingGateError {
    InvalidIdentifier {
        name: String,
    },
    PlaceholderIdentifier {
        name: String,
    },
    MissingOrigin,
    MissingEvidence {
        origin: String,
        name: String,
    },
    InvalidModulePath {
        path: String,
        reason: &'static str,
    },
    UnknownTokens {
        name: String,
        origin: String,
        tokens: Vec<String>,
    },
}

impl NamingGateError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::InvalidIdentifier { name } => {
                format!("semantic name {name} is not a valid JavaScript identifier")
            }
            Self::PlaceholderIdentifier { name } => {
                format!(
                    "semantic name {name} is a generated placeholder, not an accepted semantic name"
                )
            }
            Self::MissingOrigin => "naming provenance requires a non-empty origin".to_string(),
            Self::MissingEvidence { origin, name } => {
                format!("automated origin {origin} must provide evidence for semantic name {name}")
            }
            Self::InvalidModulePath { path, reason } => {
                format!("module semantic path {path} is invalid: {reason}")
            }
            Self::UnknownTokens {
                name,
                origin,
                tokens,
            } => format!(
                "automated origin {origin} proposed semantic name {name} with token(s) absent from evidence/technical vocabulary: {}",
                tokens.join(", ")
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamingGateMode {
    /// Direct proposal/acceptance from the module-level symbol workflow.
    Symbol,
    /// Generated-output local binding workflow. Local bindings are allowed a
    /// slightly broader role vocabulary because many are only temporary values.
    LocalBinding,
}

pub(crate) fn validate_name_acceptance(
    original_name: &str,
    semantic_name: &str,
    origin: &str,
    evidence: Option<&str>,
    mode: NamingGateMode,
) -> Result<(), NamingGateError> {
    validate_identifier(semantic_name)?;
    validate_provenance(semantic_name, origin, evidence)?;
    if is_automated_origin(origin) && semantic_name != original_name {
        validate_vocabulary(semantic_name, origin, evidence.unwrap_or_default(), mode)?;
    }
    Ok(())
}

pub(crate) fn validate_module_path_acceptance(
    semantic_path: &str,
    origin: &str,
) -> Result<(), NamingGateError> {
    validate_origin(origin)?;
    if semantic_path.trim().is_empty() {
        return Err(NamingGateError::InvalidModulePath {
            path: semantic_path.to_string(),
            reason: "empty path",
        });
    }
    if semantic_path.starts_with('/') {
        return Err(NamingGateError::InvalidModulePath {
            path: semantic_path.to_string(),
            reason: "absolute paths are not allowed",
        });
    }
    if semantic_path.contains('\\') {
        return Err(NamingGateError::InvalidModulePath {
            path: semantic_path.to_string(),
            reason: "backslashes are not allowed",
        });
    }
    if semantic_path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(NamingGateError::InvalidModulePath {
            path: semantic_path.to_string(),
            reason: "empty, dot, and dot-dot path segments are not allowed",
        });
    }
    Ok(())
}

pub(crate) fn is_automated_name_origin(origin: &str) -> bool {
    is_automated_origin(origin)
}

pub(crate) fn evidence_tokens(value: &str) -> Vec<String> {
    let mut tokens = split_words(value);
    tokens.sort();
    tokens.dedup();
    tokens
}

fn validate_identifier(name: &str) -> Result<(), NamingGateError> {
    if sanitize_identifier(name) != name {
        return Err(NamingGateError::InvalidIdentifier {
            name: name.to_string(),
        });
    }
    if is_generated_placeholder_identifier(name) {
        return Err(NamingGateError::PlaceholderIdentifier {
            name: name.to_string(),
        });
    }
    Ok(())
}

fn validate_provenance(
    semantic_name: &str,
    origin: &str,
    evidence: Option<&str>,
) -> Result<(), NamingGateError> {
    let origin = origin.trim();
    validate_origin(origin)?;
    if is_automated_origin(origin) && evidence.is_none_or(|value| value.trim().is_empty()) {
        return Err(NamingGateError::MissingEvidence {
            origin: origin.to_string(),
            name: semantic_name.to_string(),
        });
    }
    Ok(())
}

fn validate_origin(origin: &str) -> Result<(), NamingGateError> {
    if origin.trim().is_empty() {
        return Err(NamingGateError::MissingOrigin);
    }
    Ok(())
}

fn validate_vocabulary(
    semantic_name: &str,
    origin: &str,
    evidence: &str,
    mode: NamingGateMode,
) -> Result<(), NamingGateError> {
    let allowed = allowed_tokens(evidence, mode);
    let unknown = split_identifier_tokens(semantic_name)
        .into_iter()
        .filter(|token| !allowed.contains(token))
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(NamingGateError::UnknownTokens {
            name: semantic_name.to_string(),
            origin: origin.trim().to_string(),
            tokens: unknown,
        })
    }
}

fn is_automated_origin(origin: &str) -> bool {
    let normalized = origin.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "agent"
            | "ai"
            | "llm"
            | "model"
            | "openai"
            | "gpt"
            | "chatgpt"
            | "claude"
            | "anthropic"
            | "gemini"
            | "copilot"
    )
}

fn allowed_tokens(evidence: &str, mode: NamingGateMode) -> BTreeSet<String> {
    let mut tokens = technical_and_role_tokens(mode)
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    tokens.extend(evidence_tokens(evidence));
    tokens
}

fn technical_and_role_tokens(mode: NamingGateMode) -> Vec<&'static str> {
    let mut tokens = vec![
        // Common operation verbs. These are role words, not business-domain
        // words; domain nouns must come from evidence.
        "add",
        "append",
        "apply",
        "build",
        "call",
        "clear",
        "clone",
        "collect",
        "compile",
        "configure",
        "convert",
        "copy",
        "create",
        "decode",
        "delete",
        "emit",
        "encode",
        "execute",
        "extract",
        "fetch",
        "find",
        "format",
        "get",
        "handle",
        "has",
        "init",
        "initialize",
        "insert",
        "is",
        "join",
        "load",
        "make",
        "map",
        "merge",
        "normalize",
        "open",
        "parse",
        "print",
        "process",
        "read",
        "remove",
        "render",
        "replace",
        "request",
        "reset",
        "resolve",
        "return",
        "run",
        "save",
        "select",
        "send",
        "serialize",
        "set",
        "sort",
        "split",
        "store",
        "to",
        "transform",
        "update",
        "use",
        "validate",
        "walk",
        "write",
        // Technical nouns and generic roles that do not introduce business facts.
        "adapter",
        "args",
        "argument",
        "array",
        "asset",
        "ast",
        "binding",
        "boolean",
        "buffer",
        "bundle",
        "callback",
        "class",
        "client",
        "code",
        "config",
        "context",
        "ctx",
        "data",
        "db",
        "document",
        "entry",
        "error",
        "event",
        "export",
        "file",
        "filter",
        "fn",
        "function",
        "graph",
        "handler",
        "id",
        "identifier",
        "import",
        "index",
        "input",
        "item",
        "iterator",
        "json",
        "key",
        "list",
        "map",
        "message",
        "module",
        "name",
        "node",
        "object",
        "option",
        "options",
        "output",
        "package",
        "params",
        "parser",
        "path",
        "payload",
        "plan",
        "project",
        "promise",
        "property",
        "record",
        "ref",
        "request",
        "response",
        "result",
        "row",
        "runtime",
        "scope",
        "source",
        "state",
        "stmt",
        "string",
        "symbol",
        "target",
        "token",
        "type",
        "value",
        "visitor",
        "worker",
        // JS/platform terms.
        "asar",
        "async",
        "await",
        "bunfs",
        "css",
        "dom",
        "electron",
        "html",
        "ipc",
        "js",
        "jsx",
        "nodejs",
        "npm",
        "oxc",
        "react",
        "rollup",
        "ts",
        "tsx",
        "uuid",
        "vite",
        "wasm",
        "webpack",
        "xml",
        "yaml",
    ];
    if mode == NamingGateMode::LocalBinding {
        tokens.extend([
            "current", "first", "last", "left", "next", "previous", "raw", "right", "temp",
        ]);
    }
    tokens
}

fn split_identifier_tokens(value: &str) -> Vec<String> {
    split_words(value)
}

fn split_words(value: &str) -> Vec<String> {
    let mut normalized = String::new();
    let mut previous_lower_or_digit = false;
    for character in value.chars() {
        if character.is_ascii_uppercase() && previous_lower_or_digit {
            normalized.push(' ');
        }
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
            previous_lower_or_digit = character.is_ascii_lowercase() || character.is_ascii_digit();
        } else {
            normalized.push(' ');
            previous_lower_or_digit = false;
        }
    }
    normalized
        .split_whitespace()
        .filter(|token| !token.chars().all(|character| character.is_ascii_digit()))
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{NamingGateError, NamingGateMode, validate_name_acceptance};

    #[test]
    fn automated_names_require_evidence() {
        let error =
            validate_name_acceptance("a", "refreshToken", "agent", None, NamingGateMode::Symbol)
                .expect_err("automated names need evidence");
        assert!(matches!(error, NamingGateError::MissingEvidence { .. }));
    }

    #[test]
    fn automated_names_reject_tokens_absent_from_evidence() {
        let error = validate_name_acceptance(
            "a",
            "billingInvoiceHandler",
            "agent",
            Some("route:/api/session handler"),
            NamingGateMode::Symbol,
        )
        .expect_err("billing and invoice are unsupported");
        assert!(matches!(error, NamingGateError::UnknownTokens { .. }));
    }

    #[test]
    fn automated_names_accept_tokens_from_evidence() {
        validate_name_acceptance(
            "a",
            "refreshAccessToken",
            "agent",
            Some("string:refresh_token string:access_token calls:fetch"),
            NamingGateMode::Symbol,
        )
        .expect("tokens are evidence backed");
    }

    #[test]
    fn manual_names_do_not_require_evidence() {
        validate_name_acceptance(
            "a",
            "billingInvoiceHandler",
            "human",
            None,
            NamingGateMode::Symbol,
        )
        .expect("manual review is accepted provenance");
    }
}
