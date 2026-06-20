//! Topological de-lazification of esbuild `__esm` lazy module-init thunks.
//!
//! esbuild emits each ESM module's top-level code inside a lazily-memoized init
//! thunk: `var X = lazyValue(() => { ...module body... })`, invoked as `X()` by
//! importers before they touch the module's exports. The decompiler preserves
//! that shape. This pass un-wraps it for modules that don't need laziness:
//!
//!   `var X = lazyValue(() => { BODY })`  →  `BODY ; function X() {}`
//!
//! The body then runs EAGERLY at module evaluation (its natural ESM position),
//! and a no-op `X` stub keeps every existing `X()` call site valid (the init
//! already ran on import). No cross-module call-site rewriting is needed.
//!
//! Soundness — the ONLY hazard is initialization CYCLES. ECMAScript evaluates
//! modules in topological IMPORT order, so a module whose import-closure is
//! acyclic has all its dependencies fully evaluated before its own body runs;
//! eager init is then exactly what plain ESM would do. A module inside an import
//! cycle REQUIRES the lazy memoization to break init recursion (eager init would
//! touch a not-yet-evaluated binding — observed empirically as
//! `TypeError: <thunk> is not a function`). So cyclicity is decided on the
//! MODULE IMPORT graph (Tarjan SCC over the emitted `import … from` edges), NOT
//! the thunk-call graph: a thunk is de-lazified only when its OWN file is a
//! singleton SCC with no self-import. Import detection is intentionally
//! over-inclusive (any `from '…'`/`import '…'` relative specifier) so a missed
//! edge can only ADD a false cycle and skip a candidate — never de-lazify an
//! unsafe one.
//!
//! The import-graph reasoning is valid ONLY for files that are a single ESM
//! module — i.e. esbuild's per-module `NNNN-esbuild-XXX.ts` files, where the
//! file's `import … from` edges fully capture its eval-order dependencies. The
//! runtime-helper aggregation files (`…/source-N-helpers.ts`, emitted by
//! `runtime_helpers_path`) instead MERGE the folded chunks of many modules into
//! one file; that file's own acyclicity says nothing about the relative init
//! order of the modules folded into it (that order is carried by the lazy thunks
//! themselves, not by file imports). De-lazifying a thunk inside an aggregation
//! file would run one folded module's body eagerly at helper-load, collapsing
//! that per-module ordering. So aggregation files (`*-helpers.ts`) are skipped
//! entirely — matching the planner's design that impure folded initializers keep
//! their lazy thunk.

use std::collections::{BTreeMap, BTreeSet};

use reverts_js::{is_ascii_identifier_continue, is_ascii_identifier_start};

use crate::EmitPlan;
use crate::byte_lexer::{expect_arrow, find_matching_brace, skip_non_code_at, skip_ws};
use crate::identifiers::{keyword_at, parse_identifier};

struct ThunkDecl {
    name: String,
    decl_span: (usize, usize),
    body_inner: String,
}

fn parse_thunk(source: &str, start: usize) -> Option<(String, (usize, usize), String, usize)> {
    let bytes = source.as_bytes();
    let keyword = ["var", "let", "const"]
        .into_iter()
        .find(|kw| keyword_at(source, start, kw))?;
    let mut cursor = start + keyword.len();
    cursor = skip_ws(bytes, cursor);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if !source[cursor..].starts_with("lazyValue(") {
        return None;
    }
    cursor += "lazyValue(".len();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_open = cursor;
    let body_close = find_matching_brace(source, cursor)?;
    let body_inner = source[body_open + 1..body_close].to_string();
    let after_body = skip_ws(bytes, body_close + 1);
    if bytes.get(after_body) != Some(&b')') {
        return None;
    }
    let after_paren = skip_ws(bytes, after_body + 1);
    if bytes.get(after_paren) != Some(&b';') {
        return None;
    }
    Some((
        binding.to_string(),
        (start, after_paren + 1),
        body_inner,
        after_paren + 1,
    ))
}

/// Normalize a `/`-separated path, resolving `.`/`..`, and map a trailing `.js`
/// to `.ts` (the emitted module extension).
fn normalize_module_path(dir: &str, specifier: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in dir.split('/').chain(specifier.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut path = parts.join("/");
    if let Some(stripped) = path.strip_suffix(".js") {
        path = format!("{stripped}.ts");
    } else if !path.ends_with(".ts") {
        path.push_str(".ts");
    }
    path
}

/// Resolved file paths this source imports, from every `from '…'` and bare
/// `import '…'` relative specifier.
fn imported_files(source: &str, file_path: &str) -> BTreeSet<String> {
    let dir = file_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let bytes = source.as_bytes();
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if !source.is_char_boundary(i) {
            i += 1;
            continue;
        }
        // Look for `from` or `import` immediately preceding a quoted specifier.
        let matched = (source[i..].starts_with("from") && i + 4 < bytes.len())
            || (source[i..].starts_with("import") && i + 6 < bytes.len());
        if matched {
            let kw_len = if bytes[i] == b'f' { 4 } else { 6 };
            let mut j = skip_ws(bytes, i + kw_len);
            if matches!(bytes.get(j), Some(b'\'') | Some(b'"')) {
                let quote = bytes[j];
                if let Some(rel) = source[j + 1..].find(quote as char) {
                    let spec = &source[j + 1..j + 1 + rel];
                    if spec.starts_with("./") || spec.starts_with("../") {
                        out.insert(normalize_module_path(dir, spec));
                    }
                    j = j + 1 + rel + 1;
                }
            }
            i = j.max(i + 1);
            continue;
        }
        i += 1;
    }
    out
}

/// Tarjan SCC; returns the set of node indices that are in a non-trivial SCC
/// (size > 1) or have a self-edge — the cyclic nodes.
fn cyclic_indices(n: usize, adj: &[Vec<usize>]) -> BTreeSet<usize> {
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut cyclic = BTreeSet::new();
    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        let mut call_stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(v, child_pos)) = call_stack.last() {
            if child_pos == 0 {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if child_pos < adj[v].len() {
                call_stack.last_mut().unwrap().1 += 1;
                let w = adj[v][child_pos];
                if index[w] == usize::MAX {
                    call_stack.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                if low[v] == index[v] {
                    let mut members = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        members.push(w);
                        if w == v {
                            break;
                        }
                    }
                    let self_loop = adj[v].contains(&v);
                    if members.len() > 1 || self_loop {
                        cyclic.extend(members);
                    }
                }
                call_stack.pop();
                if let Some(&(parent, _)) = call_stack.last() {
                    low[parent] = low[parent].min(low[v]);
                }
            }
        }
    }
    cyclic
}

/// Returns the number of thunks de-lazified.
pub(crate) fn delazify_init_chains(plan: &mut EmitPlan) -> usize {
    let joined: Vec<String> = plan.files.iter().map(|f| f.body.join("\n")).collect();
    let paths: Vec<String> = plan.files.iter().map(|f| f.path.clone()).collect();
    let path_index: BTreeMap<&str, usize> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i))
        .collect();

    // Module IMPORT graph (the eval-order graph).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); plan.files.len()];
    for (i, source) in joined.iter().enumerate() {
        for dep in imported_files(source, &paths[i]) {
            if let Some(&j) = path_index.get(dep.as_str()) {
                adj[i].push(j);
            }
        }
    }
    let cyclic = cyclic_indices(plan.files.len(), &adj);

    // Collect thunks only in ACYCLIC files, then rewrite.
    let mut edits_by_file: BTreeMap<usize, Vec<ThunkDecl>> = BTreeMap::new();
    for (file_index, source) in joined.iter().enumerate() {
        if cyclic.contains(&file_index) {
            continue;
        }
        // Skip runtime-helper aggregation files: their file-level acyclicity does
        // not reflect the per-module init order of the chunks folded into them.
        if paths[file_index].ends_with("-helpers.ts") {
            continue;
        }
        let bytes = source.as_bytes();
        let mut cursor = 0;
        while cursor < bytes.len() {
            if let Some(next) = skip_non_code_at(source, cursor) {
                cursor = next;
                continue;
            }
            if let Some((name, span, body_inner, after)) = parse_thunk(source, cursor) {
                edits_by_file
                    .entry(file_index)
                    .or_default()
                    .push(ThunkDecl {
                        name,
                        decl_span: span,
                        body_inner,
                    });
                cursor = after;
            } else {
                cursor += 1;
            }
        }
    }

    let mut delazified = 0usize;
    for (file_index, mut thunks) in edits_by_file {
        thunks.sort_by_key(|t| t.decl_span.0);
        let source = &joined[file_index];
        let mut out = String::with_capacity(source.len());
        let mut cursor = 0;
        for thunk in &thunks {
            if thunk.decl_span.0 < cursor {
                continue;
            }
            out.push_str(&source[cursor..thunk.decl_span.0]);
            out.push_str(&thunk.body_inner);
            out.push_str(&format!("\nfunction {}() {{}}", thunk.name));
            cursor = thunk.decl_span.1;
            delazified += 1;
        }
        out.push_str(&source[cursor..]);
        plan.files[file_index].body = vec![out];
    }
    delazified
}
