use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::RuntimePrelude;
use reverts_ir::BindingName;

use crate::byte_lexer::{find_matching_paren, skip_ws};
use crate::identifiers::{declaration_keyword_at, parse_identifier};
use crate::{
    IdentifierReadUsage, RuntimePreludeDirectImport, RuntimePreludeDirectImportKind,
    compact_js_source, contains_identifier_reference, identifier_read_facts_in_source,
    identifier_read_rename_site_is_safe, previous_token_is_keyword, sanitize_identifier_fragment,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct NodeBuiltinRequireRewrite {
    pub(crate) source: String,
    pub(crate) imports: BTreeMap<BindingName, String>,
    pub(crate) consumed_helpers: BTreeSet<BindingName>,
}

pub(crate) fn runtime_create_require_helpers(
    prelude: &RuntimePrelude,
    direct_imports: Option<&BTreeMap<BindingName, RuntimePreludeDirectImport>>,
) -> BTreeSet<BindingName> {
    let Some(direct_imports) = direct_imports else {
        return BTreeSet::new();
    };
    prelude
        .snippets
        .iter()
        .filter_map(|(binding, snippet)| {
            runtime_create_require_helper_callee(binding, snippet.source.as_str()).and_then(
                |callee| {
                    runtime_direct_import_is_create_require(direct_imports.get(&callee)?)
                        .then(|| binding.clone())
                },
            )
        })
        .collect()
}

pub(crate) fn runtime_direct_import_is_create_require(import: &RuntimePreludeDirectImport) -> bool {
    matches!(
        &import.kind,
        RuntimePreludeDirectImportKind::Named { imported } if imported == "createRequire"
    ) && matches!(import.source.as_str(), "module" | "node:module")
}

pub(crate) fn runtime_create_require_helper_callee(
    binding: &BindingName,
    source: &str,
) -> Option<BindingName> {
    let trimmed = source.trim();
    let (_keyword, after_keyword) = declaration_keyword_at(trimmed, 0)?;
    let bytes = trimmed.as_bytes();
    let mut cursor = skip_ws(bytes, after_keyword);
    let (name, after_name) = parse_identifier(trimmed, cursor)?;
    if name != binding.as_str() {
        return None;
    }
    cursor = skip_ws(bytes, after_name);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let (callee, after_callee) = parse_identifier(trimmed, cursor)?;
    cursor = skip_ws(bytes, after_callee);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    let close = find_matching_paren(trimmed, cursor)?;
    if compact_js_source(&trimmed[cursor + 1..close]) != "import.meta.url" {
        return None;
    }
    let after_call = skip_ws(bytes, close + 1);
    if bytes.get(after_call) != Some(&b';') || skip_ws(bytes, after_call + 1) != bytes.len() {
        return None;
    }
    Some(BindingName::new(callee))
}

pub(crate) fn rewrite_node_builtin_require_calls(
    source: &str,
    require_helpers: &BTreeSet<BindingName>,
    reserved_bindings: &BTreeSet<BindingName>,
) -> NodeBuiltinRequireRewrite {
    if require_helpers.is_empty() {
        return NodeBuiltinRequireRewrite {
            source: source.to_string(),
            ..Default::default()
        };
    }
    let mut reserved = reserved_bindings.clone();
    let mut imports_by_specifier = BTreeMap::<String, BindingName>::new();
    let mut edits = Vec::<(usize, usize, String)>::new();
    for fact in identifier_read_facts_in_source(source) {
        let helper = BindingName::new(fact.name.as_str());
        if !require_helpers.contains(&helper) {
            continue;
        }
        let Some((start, end, specifier)) = node_builtin_require_call_replacement(source, &fact)
        else {
            continue;
        };
        let alias = imports_by_specifier
            .entry(specifier.clone())
            .or_insert_with(|| node_builtin_import_binding(specifier.as_str(), &mut reserved))
            .clone();
        edits.push((start, end, alias.as_str().to_string()));
    }
    if edits.is_empty() {
        return NodeBuiltinRequireRewrite {
            source: source.to_string(),
            ..Default::default()
        };
    }
    edits.sort_by(|left, right| right.0.cmp(&left.0));
    let mut rewritten = source.to_string();
    for (start, end, replacement) in edits {
        rewritten.replace_range(start..end, replacement.as_str());
    }
    let imports = imports_by_specifier
        .into_iter()
        .map(|(specifier, binding)| (binding, specifier))
        .collect::<BTreeMap<_, _>>();
    let consumed_helpers = require_helpers
        .iter()
        .filter(|helper| !contains_identifier_reference(rewritten.as_str(), helper.as_str()))
        .cloned()
        .collect();
    NodeBuiltinRequireRewrite {
        source: rewritten,
        imports,
        consumed_helpers,
    }
}

pub(crate) fn rewrite_node_builtin_require_calls_with_imports(
    source: &str,
    require_helpers: &BTreeSet<BindingName>,
    imports: &BTreeMap<BindingName, String>,
) -> String {
    if require_helpers.is_empty() || imports.is_empty() {
        return source.to_string();
    }
    let aliases_by_specifier = imports
        .iter()
        .map(|(binding, specifier)| (specifier.clone(), binding.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut edits = identifier_read_facts_in_source(source)
        .into_iter()
        .filter_map(|fact| {
            let helper = BindingName::new(fact.name.as_str());
            if !require_helpers.contains(&helper) {
                return None;
            }
            let (start, end, specifier) = node_builtin_require_call_replacement(source, &fact)?;
            let alias = aliases_by_specifier.get(&specifier)?;
            Some((start, end, alias.as_str().to_string()))
        })
        .collect::<Vec<_>>();
    if edits.is_empty() {
        return source.to_string();
    }
    edits.sort_by(|left, right| right.0.cmp(&left.0));
    let mut rewritten = source.to_string();
    for (start, end, replacement) in edits {
        rewritten.replace_range(start..end, replacement.as_str());
    }
    rewritten
}

pub(crate) fn node_builtin_require_call_replacement(
    source: &str,
    fact: &IdentifierReadUsage,
) -> Option<(usize, usize, String)> {
    if !fact.is_call_callee
        || !identifier_read_rename_site_is_safe(source, fact.byte_start, fact.byte_end)
        || previous_token_is_keyword(source, fact.byte_start, "new")
    {
        return None;
    }
    let bytes = source.as_bytes();
    let open = skip_ws(bytes, fact.byte_end);
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let close = find_matching_paren(source, open)?;
    let specifier = parse_single_string_literal_argument(source, open + 1, close)?;
    let normalized = normalize_node_builtin_specifier(specifier.as_str())?;
    Some((fact.byte_start, close + 1, normalized))
}

pub(crate) fn parse_single_string_literal_argument(
    source: &str,
    start: usize,
    close: usize,
) -> Option<String> {
    let bytes = source.as_bytes();
    let cursor = skip_ws(bytes, start);
    let quote = *bytes.get(cursor)?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    let mut end = cursor + 1;
    while end < close {
        match bytes[end] {
            byte if byte == quote => break,
            b'\\' => return None,
            _ => end += 1,
        }
    }
    if bytes.get(end) != Some(&quote) {
        return None;
    }
    if skip_ws(bytes, end + 1) != close {
        return None;
    }
    Some(source[cursor + 1..end].to_string())
}

pub(crate) fn normalize_node_builtin_specifier(specifier: &str) -> Option<String> {
    let bare = specifier.strip_prefix("node:").unwrap_or(specifier);
    is_node_builtin_specifier(bare).then(|| format!("node:{bare}"))
}

pub(crate) fn is_node_builtin_specifier(specifier: &str) -> bool {
    matches!(
        specifier,
        "_http_agent"
            | "_http_client"
            | "_http_common"
            | "_http_incoming"
            | "_http_outgoing"
            | "_http_server"
            | "_stream_duplex"
            | "_stream_passthrough"
            | "_stream_readable"
            | "_stream_transform"
            | "_stream_wrap"
            | "_stream_writable"
            | "_tls_common"
            | "_tls_wrap"
            | "assert"
            | "assert/strict"
            | "async_hooks"
            | "buffer"
            | "child_process"
            | "cluster"
            | "console"
            | "constants"
            | "crypto"
            | "dgram"
            | "diagnostics_channel"
            | "dns"
            | "dns/promises"
            | "domain"
            | "events"
            | "fs"
            | "fs/promises"
            | "http"
            | "http2"
            | "https"
            | "inspector"
            | "inspector/promises"
            | "module"
            | "net"
            | "os"
            | "path"
            | "path/posix"
            | "path/win32"
            | "perf_hooks"
            | "process"
            | "punycode"
            | "querystring"
            | "readline"
            | "readline/promises"
            | "repl"
            | "sea"
            | "sqlite"
            | "stream"
            | "stream/consumers"
            | "stream/promises"
            | "stream/web"
            | "string_decoder"
            | "sys"
            | "test"
            | "test/reporters"
            | "timers"
            | "timers/promises"
            | "tls"
            | "trace_events"
            | "tty"
            | "url"
            | "util"
            | "util/types"
            | "v8"
            | "vm"
            | "wasi"
            | "worker_threads"
            | "zlib"
    )
}

pub(crate) fn node_builtin_import_binding(
    specifier: &str,
    reserved: &mut BTreeSet<BindingName>,
) -> BindingName {
    let bare = specifier.strip_prefix("node:").unwrap_or(specifier);
    let sanitized = sanitize_identifier_fragment(bare);
    let base = if sanitized.is_empty() {
        "node_builtin".to_string()
    } else {
        format!("node_{sanitized}")
    };
    for suffix in 0.. {
        let candidate = if suffix == 0 {
            BindingName::new(base.as_str())
        } else {
            BindingName::new(format!("{base}_{suffix}"))
        };
        if reserved.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("unbounded node builtin import alias search should always find an identifier")
}
