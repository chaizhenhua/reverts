//! Topological de-lazification of esbuild `__esm` lazy module-init thunks.
//!
//! esbuild emits each ESM module's top-level code inside a lazily-memoized init
//! thunk: `var X = MEMO(() => { ...module body... })`, invoked as `X()` by
//! importers before they touch the module's exports, where `MEMO` is the esbuild
//! `__esm` helper. The planner emits that helper in two interchangeable forms:
//! imported (`import { lazyValue } from './lazy.js'`) or inlined per-file
//! (`var _$l = (a, b) => () => (a && (b = a(a = 0)), b);`). This pass recognizes
//! BOTH by resolving the memoizer name structurally — the imported alias plus any
//! file-local definition matched by its exact signature (see [`parse_memoizer_decl`])
//! — rather than hard-coding `lazyValue`/`_$l`, then matching call sites by that
//! name. The decompiler preserves that shape. This pass un-wraps it for modules
//! that don't need laziness:
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
//!
//! Second soundness hazard — RETURN VALUE USE. The rewrite replaces the thunk
//! with a no-op `function X() {}` (returning `undefined`). The memoizer returns
//! the FACTORY's return value, so a factory `() => { STATEMENTS }` whose block
//! body has no top-level `return` yields `undefined` — identical to the no-op
//! stub for EVERY call site, whether the result is discarded (`init_X();`,
//! `(init_X(), realExport)`) or bound (`var v = X();`, which was already
//! `undefined`). The same memoizer is also used for lazily-initialized VALUE
//! exports (`() => { return VALUE }`); those DO return a value, so a top-level
//! `return` in the body blocks de-lazification ([`body_has_toplevel_return`]).
//! A thunk must additionally be invoked somewhere ([`called_names`]) — this
//! rules out a never-called thunk whose side effects must not start running
//! eagerly, and an export only an external (unseen) consumer might call. Top-
//! level emitted names are globally unique (esbuild renames on bundle), so this
//! name-keyed analysis does not conflate distinct bindings.

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

struct FileEdits {
    thunks: Vec<ThunkDecl>,
    /// File-local memoizer definitions (name + decl span) — candidates for
    /// removal once their thunks are de-lazified and they go dead.
    local_memoizers: Vec<(String, (usize, usize))>,
}

/// Parse `var X = MEMO(() => { BODY });` where `MEMO` is any name in
/// `memoizers` (the imported `lazyValue` alias and/or the file-local inlined
/// memoizer captured by signature). Returns `(binding, decl_span, body, after)`.
fn parse_thunk(
    source: &str,
    start: usize,
    memoizers: &BTreeSet<String>,
) -> Option<(String, (usize, usize), String, usize)> {
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
    // Callee must be a recognized lazy memoizer (by name, where the names were
    // resolved structurally — by import alias or signature — not hard-coded).
    let (callee, after_callee) = parse_identifier(source, cursor)?;
    if !memoizers.contains(callee) {
        return None;
    }
    cursor = skip_ws(bytes, after_callee);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
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

/// Split a leading ASCII identifier off the front of `s`, returning
/// `(identifier, rest)`.
fn take_ident(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !is_ascii_identifier_start(bytes[0]) {
        return None;
    }
    let mut i = 1;
    while i < bytes.len() && is_ascii_identifier_continue(bytes[i]) {
        i += 1;
    }
    Some((&s[..i], &s[i..]))
}

/// Recognize a file-local lazy memoizer DEFINITION by its exact structural
/// signature (esbuild's `__esm` helper, inlined per-file by the planner):
///
///   `var NAME = (A, B) => () => (A && (B = A(A = 0)), B);`
///
/// Returns `(NAME, decl_span)`. The match is name-independent — the memoizer
/// variable and both parameters may carry any (minified) identifiers; only the
/// shape is fixed. This is what lets call sites be matched without hard-coding
/// `lazyValue`/`_$l`.
fn parse_memoizer_decl(source: &str, start: usize) -> Option<(String, (usize, usize))> {
    let bytes = source.as_bytes();
    let keyword = ["var", "let", "const"]
        .into_iter()
        .find(|kw| keyword_at(source, start, kw))?;
    let mut cursor = start + keyword.len();
    cursor = skip_ws(bytes, cursor);
    let (name, after_name) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_name);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    let rhs_start = skip_ws(bytes, cursor + 1);
    // The RHS is a parenthesis-balanced arrow expression; its statement
    // terminator is the first `;` at paren depth 0.
    let mut i = rhs_start;
    let mut depth = 0usize;
    let semi = loop {
        if i >= bytes.len() {
            return None;
        }
        if let Some(next) = skip_non_code_at(source, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.checked_sub(1)?,
            b';' if depth == 0 => break i,
            _ => {}
        }
        i += 1;
    };
    let compact: String = source[rhs_start..semi]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    // Expect exactly `(A,B)=>()=>(A&&(B=A(A=0)),B)`.
    let inner = compact.strip_prefix('(')?;
    let (a, rest) = take_ident(inner)?;
    let rest = rest.strip_prefix(',')?;
    let (b, rest) = take_ident(rest)?;
    let rest = rest.strip_prefix(")=>()=>(")?;
    if rest != format!("{a}&&({b}={a}({a}=0)),{b})") {
        return None;
    }
    Some((name.to_string(), (start, semi + 1)))
}

/// All file-local memoizer definitions, found by signature.
fn local_memoizer_decls(source: &str) -> Vec<(String, (usize, usize))> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if let Some((name, span)) = parse_memoizer_decl(source, cursor) {
            out.push((name, span));
            cursor = span.1;
        } else {
            cursor += 1;
        }
    }
    out
}

/// Local names under which the shared `lazyValue` memoizer is imported (from a
/// `…/lazy.js`/`.ts` module). Handles `lazyValue` and `lazyValue as Local`.
fn imported_lazy_value_names(source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("import") {
            continue;
        }
        if !(trimmed.contains("/lazy.js") || trimmed.contains("/lazy.ts")) {
            continue;
        }
        let (Some(open), Some(close)) = (trimmed.find('{'), trimmed.find('}')) else {
            continue;
        };
        if open >= close {
            continue;
        }
        for part in trimmed[open + 1..close].split(',') {
            let mut tokens = part.split_whitespace();
            if tokens.next() != Some("lazyValue") {
                continue;
            }
            let local = match (tokens.next(), tokens.next()) {
                (Some("as"), Some(alias)) => alias,
                _ => "lazyValue",
            };
            names.insert(local.to_string());
        }
    }
    names
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

/// True if `body` (a thunk factory's block body) contains a `return` at the
/// factory's own statement level — i.e. NOT inside a nested function/arrow block.
/// The memoizer returns the factory's return value, so a body with no top-level
/// `return` yields `undefined`, exactly like the no-op stub. Brace depth tracks
/// nesting: a nested function body is `{…}`, so its `return` sits at depth > 0.
fn body_has_toplevel_return(body: &str) -> bool {
    let bytes = body.as_bytes();
    let mut cursor = 0;
    let mut depth = 0i32;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(body, cursor) {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ if depth == 0
                && keyword_at(body, cursor, "return")
                && (cursor == 0 || !is_ascii_identifier_continue(bytes[cursor - 1])) =>
            {
                return true;
            }
            _ => {}
        }
        cursor += 1;
    }
    false
}

/// Every name that appears as a call `NAME(…)` anywhere in the program. Used to
/// confirm a thunk is actually invoked before its body is hoisted to eager
/// module-eval (so a never-called thunk's side effects aren't introduced). Top-
/// level emitted names are globally unique (esbuild renames on bundle), so a name
/// match is not conflated across modules.
fn called_names(joined: &[String]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for source in joined {
        let bytes = source.as_bytes();
        let mut cursor = 0;
        while cursor < bytes.len() {
            if let Some(next) = skip_non_code_at(source, cursor) {
                cursor = next;
                continue;
            }
            let at_word_boundary = is_ascii_identifier_start(bytes[cursor])
                && (cursor == 0
                    || (!is_ascii_identifier_continue(bytes[cursor - 1])
                        && bytes[cursor - 1] != b'.'));
            if at_word_boundary {
                if let Some((name, after)) = parse_identifier(source, cursor) {
                    if bytes.get(skip_ws(bytes, after)) == Some(&b'(') {
                        names.insert(name.to_string());
                    }
                    cursor = after;
                    continue;
                }
            }
            cursor += 1;
        }
    }
    names
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

    // Whole-program set of invoked names (return-value-use gate companion).
    let called = called_names(&joined);

    // Collect thunks only in ACYCLIC files, then rewrite.
    let mut edits_by_file: BTreeMap<usize, FileEdits> = BTreeMap::new();
    for (file_index, source) in joined.iter().enumerate() {
        if cyclic.contains(&file_index) {
            continue;
        }
        // Skip runtime-helper aggregation files: their file-level acyclicity does
        // not reflect the per-module init order of the chunks folded into them.
        if paths[file_index].ends_with("-helpers.ts") {
            continue;
        }
        // Resolve which callee names denote a lazy memoizer in THIS file: the
        // imported `lazyValue` alias(es) and any file-local memoizer captured by
        // structural signature. No name is hard-coded.
        let local_memoizers = local_memoizer_decls(source);
        let mut memoizer_names = imported_lazy_value_names(source);
        memoizer_names.extend(local_memoizers.iter().map(|(name, _)| name.clone()));
        if memoizer_names.is_empty() {
            continue;
        }
        let bytes = source.as_bytes();
        let mut cursor = 0;
        let mut thunks = Vec::new();
        while cursor < bytes.len() {
            if let Some(next) = skip_non_code_at(source, cursor) {
                cursor = next;
                continue;
            }
            if let Some((name, span, body_inner, after)) =
                parse_thunk(source, cursor, &memoizer_names)
            {
                // Gate 2: de-lazify only with PROOF the eager rewrite is
                // observationally equivalent — the thunk must be invoked
                // somewhere (so its body provably runs, not a never-called or
                // externally-only export) AND its factory body has no top-level
                // `return` (so it yields `undefined`, exactly like the no-op
                // stub, no matter how any call site uses the result).
                if called.contains(&name) && !body_has_toplevel_return(&body_inner) {
                    thunks.push(ThunkDecl {
                        name,
                        decl_span: span,
                        body_inner,
                    });
                }
                cursor = after;
            } else {
                cursor += 1;
            }
        }
        if thunks.is_empty() {
            continue;
        }
        edits_by_file.insert(
            file_index,
            FileEdits {
                thunks,
                local_memoizers,
            },
        );
    }

    let mut delazified = 0usize;
    for (file_index, mut edits) in edits_by_file {
        edits.thunks.sort_by_key(|t| t.decl_span.0);
        let source = &joined[file_index];
        let mut out = String::with_capacity(source.len());
        let mut cursor = 0;
        for thunk in &edits.thunks {
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

        // Drop any file-local memoizer that is now dead: with every thunk
        // de-lazified, an inlined `var _$l = …;` helper has no remaining
        // `_$l(` call site. (Guarded by an actual usage check so a memoizer
        // still referenced elsewhere is preserved.)
        for (name, span) in &edits.local_memoizers {
            if out.contains(&format!("{name}(")) {
                continue;
            }
            let decl_text = &source[span.0..span.1];
            let with_newline = format!("{decl_text}\n");
            if out.contains(&with_newline) {
                out = out.replacen(&with_newline, "", 1);
            } else {
                out = out.replacen(decl_text, "", 1);
            }
        }
        plan.files[file_index].body = vec![out];
    }
    delazified
}
