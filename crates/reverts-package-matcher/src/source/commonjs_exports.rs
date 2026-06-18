//! Text-based supplemental scanner for CommonJS-style export members. Used when
//! the AST-driven [`ExportMemberCollector`](crate::source::exported_members::ExportMemberCollector)
//! path is unavailable or as a supplemental signal for sources that mix
//! ES module and CommonJS export shapes.

use std::collections::BTreeSet;

use super::source_text::{
    compact_ascii_ws, read_identifier_at, read_identifier_with_end_at, read_quoted_string_at,
    skip_ascii_ws,
};

#[must_use]
pub(crate) fn commonjs_export_members_from_text(source: &str) -> BTreeSet<String> {
    let mut members = BTreeSet::new();
    collect_member_assignments_from_text(source, "exports.", &mut members);
    collect_member_assignments_from_text(source, "module.exports.", &mut members);
    collect_define_property_members_from_text(source, "exports", &mut members);
    collect_define_property_members_from_text(source, "module.exports", &mut members);
    collect_create_binding_members_from_text(source, &mut members);
    collect_module_exports_named_value_from_text(source, &mut members);
    members
}

fn collect_member_assignments_from_text(
    source: &str,
    prefix: &str,
    members: &mut BTreeSet<String>,
) {
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find(prefix) {
        let start = cursor + relative + prefix.len();
        let Some(member) = read_identifier_at(source, start) else {
            cursor = start;
            continue;
        };
        let after = start + member.len();
        let after_ws = skip_ascii_ws(source.as_bytes(), after);
        if source.as_bytes().get(after_ws) == Some(&b'=') {
            members.insert(member.to_string());
        }
        cursor = after;
    }
}

fn collect_define_property_members_from_text(
    source: &str,
    object: &str,
    members: &mut BTreeSet<String>,
) {
    let needle = format!("Object.defineProperty({object},");
    let compact = compact_ascii_ws(source);
    let mut cursor = 0;
    while let Some(relative) = compact[cursor..].find(needle.as_str()) {
        let start = cursor + relative + needle.len();
        let start = skip_ascii_ws(compact.as_bytes(), start);
        let Some((member, end)) = read_quoted_string_at(compact.as_str(), start) else {
            cursor = start;
            continue;
        };
        members.insert(member);
        cursor = end;
    }
}

fn collect_create_binding_members_from_text(source: &str, members: &mut BTreeSet<String>) {
    let compact = compact_ascii_ws(source);
    let mut cursor = 0;
    while let Some(relative) = compact[cursor..].find("__createBinding(") {
        let start = cursor + relative + "__createBinding(".len();
        let statement_end = compact[start..]
            .find(';')
            .map(|offset| start + offset)
            .unwrap_or(compact.len());
        let call = &compact[start..statement_end];
        if !(call.starts_with("exports,") || call.starts_with("module.exports,")) {
            cursor = statement_end.saturating_add(1).min(compact.len());
            continue;
        }
        let Some(last_comma) = call.rfind(',') else {
            cursor = statement_end.saturating_add(1).min(compact.len());
            continue;
        };
        if let Some((member, _end)) = read_quoted_string_at(call, last_comma + 1) {
            members.insert(member);
        }
        cursor = statement_end.saturating_add(1).min(compact.len());
    }
}

fn collect_module_exports_named_value_from_text(source: &str, members: &mut BTreeSet<String>) {
    let compact = compact_ascii_ws(source);
    let mut cursor = 0;
    while let Some(relative) = compact[cursor..].find("module.exports=") {
        let start = cursor + relative + "module.exports=".len();
        let Some((identifier, end)) = read_identifier_with_end_at(compact.as_str(), start) else {
            cursor = start;
            continue;
        };
        let next = compact.as_bytes().get(end).copied();
        if next != Some(b'(')
            && !matches!(
                identifier,
                "require" | "__importStar" | "__importDefault" | "__toESM" | "Object"
            )
        {
            members.insert(identifier.to_string());
        }
        cursor = end;
    }
}
