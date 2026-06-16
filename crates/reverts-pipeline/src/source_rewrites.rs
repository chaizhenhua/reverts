//! Post-emit source-text rewrites.
//!
//! These passes run on the already-rendered `EmittedProject` source
//! strings. They are still AST-driven — every rewrite locates the exact
//! span via OXC (`collect_static_template_literals`,
//! `collect_file_url_source_location_rewrites`,
//! `collect_string_literals`) and then splices the replacement back into
//! the source. None of this is regex or naive string search; ADR 0001
//! forbids that and these passes comply.
//!
//! - `canonicalize_emitted_source_locations` normalizes the `file://`
//!   URLs OXC's codegen emits for `import.meta.url` back into the
//!   `import.meta.url` form a real runtime would use.
//! - `fold_multiline_static_template_literals` collapses two-or-more
//!   line static template literals to double-quoted strings. The
//!   bundler's escape-folding makes these multiline by accident; we
//!   keep them readable without changing semantics.
//! - `rewrite_string_literal_values` is the targeted rewrite the asset
//!   pass uses: given a map of old-value → new-value, replace any
//!   matching string literal in single-quoted form.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use reverts_emitter::EmittedProject;
use reverts_js::{
    ParseGoal, collect_file_url_source_location_rewrites, collect_static_template_literals,
    collect_string_literals,
};

pub(crate) fn canonicalize_emitted_source_locations(project: &mut EmittedProject) {
    for file in &mut project.files {
        file.source = rewrite_file_url_source_locations(file.source.as_str(), file.path.as_str());
    }
}

const STATIC_TEMPLATE_LITERAL_FOLD_MIN_LINES: usize = 2;

pub(crate) fn fold_multiline_static_template_literals(project: &mut EmittedProject) {
    for file in &mut project.files {
        if !file.source.contains('`') {
            continue;
        }
        file.source = fold_multiline_static_template_literals_in_source(
            file.source.as_str(),
            file.path.as_str(),
        );
    }
}

pub(crate) fn fold_multiline_static_template_literals_in_source(
    source: &str,
    path_hint: &str,
) -> String {
    let Ok(literals) =
        collect_static_template_literals(source, Some(Path::new(path_hint)), ParseGoal::TypeScript)
    else {
        return source.to_string();
    };
    let mut replacements = literals
        .into_iter()
        .filter_map(|literal| {
            let start = literal.byte_start as usize;
            let end = literal.byte_end as usize;
            let raw = source.get(start..end)?;
            let raw_line_count = raw.as_bytes().iter().filter(|byte| **byte == b'\n').count() + 1;
            (raw_line_count >= STATIC_TEMPLATE_LITERAL_FOLD_MIN_LINES)
                .then(|| (start, end, double_quoted_js_string(literal.value.as_str())))
        })
        .collect::<Vec<_>>();
    if replacements.is_empty() {
        return source.to_string();
    }
    replacements.sort_by_key(|(start, _, _)| *start);

    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end, replacement) in replacements {
        if start < cursor || end < start || end > source.len() {
            return source.to_string();
        }
        output.push_str(&source[cursor..start]);
        output.push_str(replacement.as_str());
        cursor = end;
    }
    output.push_str(&source[cursor..]);
    output
}

fn rewrite_file_url_source_locations(source: &str, path_hint: &str) -> String {
    let Ok(rewrites) = collect_file_url_source_location_rewrites(
        source,
        Some(Path::new(path_hint)),
        ParseGoal::TypeScript,
    ) else {
        return source.to_string();
    };
    let mut output = source.to_string();
    for rewrite in rewrites.iter().rev() {
        output.replace_range(
            rewrite.byte_start as usize..rewrite.byte_end as usize,
            "import.meta.url",
        );
    }
    output
}

pub(crate) fn rewrite_string_literal_values(
    source: &str,
    path_hint: &str,
    rewrites: &BTreeMap<String, String>,
) -> String {
    let Ok(literals) =
        collect_string_literals(source, Some(Path::new(path_hint)), ParseGoal::TypeScript)
    else {
        return source.to_string();
    };
    let mut output = source.to_string();
    for literal in literals.iter().rev() {
        let Some(replacement) = rewrites.get(literal.value.as_str()) else {
            continue;
        };
        output.replace_range(
            literal.byte_start as usize..literal.byte_end as usize,
            single_quoted_js_string(replacement).as_str(),
        );
    }
    output
}

fn double_quoted_js_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0C}' => output.push_str("\\f"),
            '\u{2028}' => output.push_str("\\u2028"),
            '\u{2029}' => output.push_str("\\u2029"),
            ch if ch.is_control() => {
                write!(output, "\\u{:04X}", ch as u32).expect("writing to String should not fail");
            }
            _ => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn single_quoted_js_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('\'');
    for ch in value.chars() {
        match ch {
            '\'' => output.push_str("\\'"),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            _ => output.push(ch),
        }
    }
    output.push('\'');
    output
}
