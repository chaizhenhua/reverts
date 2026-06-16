//! Text-based collectors that recover relative-import / require / re-export
//! targets from minified or unparseable package source bodies. Used by the
//! external-import resolver and the structural-bag matcher to walk
//! reexport graphs without re-parsing each source.

use std::collections::BTreeSet;

use crate::source_text::{compact_ascii_ws, read_quoted_string_at};

#[must_use]
pub(crate) fn relative_module_specifier_targets(source: &str) -> BTreeSet<String> {
    let compact = compact_ascii_ws(source);
    let mut targets = BTreeSet::new();
    collect_require_targets_from_compact_slice(compact.as_str(), &mut targets);
    collect_static_import_export_targets(compact.as_str(), &mut targets);
    targets
}

fn collect_static_import_export_targets(source: &str, targets: &mut BTreeSet<String>) {
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("from") {
        let start = cursor + relative + "from".len();
        let Some((target, end)) = read_quoted_string_at(source, start) else {
            cursor = start;
            continue;
        };
        if target.starts_with('.') {
            targets.insert(target);
        }
        cursor = end;
    }

    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("import") {
        let start = cursor + relative + "import".len();
        let start = if source.as_bytes().get(start) == Some(&b'(') {
            start + 1
        } else {
            start
        };
        let Some((target, end)) = read_quoted_string_at(source, start) else {
            cursor = start;
            continue;
        };
        if target.starts_with('.') {
            targets.insert(target);
        }
        cursor = end;
    }
}

#[must_use]
pub(crate) fn export_all_reexport_targets(source: &str) -> BTreeSet<String> {
    let compact = compact_ascii_ws(source);
    let mut targets = BTreeSet::new();
    collect_export_all_declaration_targets(compact.as_str(), &mut targets);
    collect_export_star_helper_targets(compact.as_str(), &mut targets);
    collect_commonjs_module_exports_require_targets(compact.as_str(), &mut targets);
    targets
}

#[must_use]
pub(crate) fn reexport_targets(source: &str) -> BTreeSet<String> {
    let compact = compact_ascii_ws(source);
    let mut targets = BTreeSet::new();
    collect_export_all_declaration_targets(compact.as_str(), &mut targets);
    collect_export_named_declaration_targets(compact.as_str(), &mut targets);
    collect_export_star_helper_targets(compact.as_str(), &mut targets);
    collect_commonjs_reexport_targets(compact.as_str(), &mut targets);
    targets
}

fn collect_export_all_declaration_targets(source: &str, targets: &mut BTreeSet<String>) {
    let needle = "export*from";
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find(needle) {
        let start = cursor + relative + needle.len();
        let Some((target, end)) = read_quoted_string_at(source, start) else {
            cursor = start;
            continue;
        };
        if target.starts_with('.') {
            targets.insert(target);
        }
        cursor = end;
    }
}

fn collect_export_named_declaration_targets(source: &str, targets: &mut BTreeSet<String>) {
    let needle = "export{";
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find(needle) {
        let start = cursor + relative + needle.len();
        let Some(close_relative) = source[start..].find("}from") else {
            cursor = start;
            continue;
        };
        let quote_start = start + close_relative + "}from".len();
        let Some((target, end)) = read_quoted_string_at(source, quote_start) else {
            cursor = quote_start;
            continue;
        };
        if target.starts_with('.') {
            targets.insert(target);
        }
        cursor = end;
    }
}

fn collect_export_star_helper_targets(source: &str, targets: &mut BTreeSet<String>) {
    for helper in ["__exportStar(", "__export("] {
        let mut cursor = 0;
        while let Some(relative) = source[cursor..].find(helper) {
            let call_start = cursor + relative + helper.len();
            let Some(require_offset) = source[call_start..].find("require(") else {
                cursor = call_start;
                continue;
            };
            let require_start = call_start + require_offset + "require(".len();
            let Some((target, end)) = read_quoted_string_at(source, require_start) else {
                cursor = require_start;
                continue;
            };
            if target.starts_with('.') {
                targets.insert(target);
            }
            cursor = end;
        }
    }
}

#[must_use]
pub(crate) fn commonjs_reexport_targets(source: &str) -> BTreeSet<String> {
    let compact = compact_ascii_ws(source);
    let mut targets = BTreeSet::new();
    collect_commonjs_reexport_targets(compact.as_str(), &mut targets);
    targets
}

fn collect_commonjs_reexport_targets(source: &str, targets: &mut BTreeSet<String>) {
    collect_commonjs_module_exports_require_targets(source, targets);
    collect_commonjs_export_member_require_targets(source, targets);
    collect_create_binding_require_targets(source, targets);
    collect_import_star_reexport_targets(source, targets);
}

fn collect_commonjs_module_exports_require_targets(source: &str, targets: &mut BTreeSet<String>) {
    let needle = "module.exports=";
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find(needle) {
        let rhs_start = cursor + relative + needle.len();
        let statement_end = source[rhs_start..]
            .find(';')
            .map(|offset| rhs_start + offset)
            .unwrap_or(source.len());
        let rhs = &source[rhs_start..statement_end];
        if rhs.starts_with("require(")
            || rhs.contains("__importStar(require(")
            || (rhs.contains("?require(") && rhs.contains(":require("))
        {
            collect_require_targets_from_compact_slice(rhs, targets);
        }
        cursor = statement_end.saturating_add(1).min(source.len());
    }
}

fn collect_commonjs_export_member_require_targets(source: &str, targets: &mut BTreeSet<String>) {
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("=require(") {
        let equals = cursor + relative;
        let statement_start = source[..equals]
            .rfind(';')
            .map(|index| index + 1)
            .unwrap_or_default();
        let lhs = &source[statement_start..equals];
        if lhs.starts_with("exports.") || lhs.starts_with("module.exports.") {
            let require_start = equals + "=require(".len();
            if let Some((target, end)) = read_quoted_string_at(source, require_start) {
                if target.starts_with('.') {
                    targets.insert(target);
                }
                cursor = end;
                continue;
            }
        }
        cursor = equals + 1;
    }
}

fn collect_create_binding_require_targets(source: &str, targets: &mut BTreeSet<String>) {
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("__createBinding(") {
        let call_start = cursor + relative + "__createBinding(".len();
        let statement_end = source[call_start..]
            .find(';')
            .map(|offset| call_start + offset)
            .unwrap_or(source.len());
        let call = &source[call_start..statement_end];
        if call.starts_with("exports,") || call.starts_with("module.exports,") {
            collect_require_targets_from_compact_slice(call, targets);
        }
        cursor = statement_end.saturating_add(1).min(source.len());
    }
}

fn collect_import_star_reexport_targets(source: &str, targets: &mut BTreeSet<String>) {
    if !(source.contains("exports.") || source.contains("module.exports.")) {
        return;
    }
    for helper in ["__importStar(require(", "__importDefault(require("] {
        let mut cursor = 0;
        while let Some(relative) = source[cursor..].find(helper) {
            let require_start = cursor + relative + helper.len();
            let Some((target, end)) = read_quoted_string_at(source, require_start) else {
                cursor = require_start;
                continue;
            };
            if target.starts_with('.') {
                targets.insert(target);
            }
            cursor = end;
        }
    }
}

fn collect_require_targets_from_compact_slice(source: &str, targets: &mut BTreeSet<String>) {
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("require(") {
        let start = cursor + relative + "require(".len();
        let Some((target, end)) = read_quoted_string_at(source, start) else {
            cursor = start;
            continue;
        };
        if target.starts_with('.') {
            targets.insert(target);
        }
        cursor = end;
    }
}
