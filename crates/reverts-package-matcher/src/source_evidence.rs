//! Shared source-evidence profiles for package, cross-version, and
//! first-party source-tree matching. The profile is intentionally name-light:
//! stable hashes, string anchors, function-axis anchors, and JSX/React shapes
//! are extracted once and scored through the same API everywhere.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    Visit,
    ast::{
        Argument, CallExpression, Expression, JSXAttributeItem, JSXAttributeName, JSXElementName,
        JSXMemberExpression, JSXMemberExpressionObject, JSXOpeningElement,
    },
    visit::walk::{walk_call_expression, walk_jsx_opening_element},
};
use oxc_parser::Parser;
use reverts_graph::FunctionExtractor;
use reverts_ir::{AxisHashes, AxisKind, FunctionFingerprint, ModuleId, NormalizationPassId};
use reverts_js::{ParseGoal, parse_options_for, source_type_candidates};

use crate::index::{SourceFingerprint, fingerprint_source};
use crate::package_helpers::normalize_hint_text;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEvidenceProfile {
    pub path: String,
    pub fingerprint: SourceFingerprint,
    pub function_axis_anchors: BTreeSet<String>,
    pub jsx_react_shape_anchors: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SourceEvidenceScore {
    pub hash_match: bool,
    pub function_axis_overlap: usize,
    pub function_axis_jaccard: f64,
    pub function_axis_containment: f64,
    pub weighted_string_anchor: f64,
    pub normalized_string_anchor: f64,
    pub unique_string_anchor_overlap: usize,
    pub jsx_react_shape_overlap: usize,
    pub jsx_react_shape_jaccard: f64,
}

impl SourceEvidenceScore {
    #[must_use]
    pub fn total(self) -> f64 {
        (if self.hash_match { 1.0e9 } else { 0.0 })
            + self.normalized_string_anchor * 150.0
            + self.weighted_string_anchor
            + self.function_axis_jaccard * 120.0
            + self.function_axis_containment * 60.0
            + (self.unique_string_anchor_overlap as f64) * 18.0
            + self.jsx_react_shape_jaccard * 80.0
            + (self.jsx_react_shape_overlap as f64) * 8.0
    }

    #[must_use]
    pub fn has_evidence(self) -> bool {
        self.hash_match
            || self.function_axis_overlap > 0
            || self.weighted_string_anchor >= 1.0
            || self.unique_string_anchor_overlap > 0
            || self.jsx_react_shape_overlap > 0
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceEvidenceIdf {
    pub string_anchors: BTreeMap<String, f64>,
    pub string_anchor_document_frequency: BTreeMap<String, usize>,
}

pub fn build_source_evidence_profile(
    path: &str,
    source: &str,
) -> Result<SourceEvidenceProfile, String> {
    let fingerprint = fingerprint_source(path, source)?;
    Ok(build_source_evidence_profile_with_fingerprint(
        path,
        source,
        fingerprint,
    ))
}

#[must_use]
pub fn build_source_evidence_profile_with_fingerprint(
    path: &str,
    source: &str,
    fingerprint: SourceFingerprint,
) -> SourceEvidenceProfile {
    let functions = FunctionExtractor::fingerprint_primary(ModuleId(0), source);
    let function_axis_anchors = function_axis_anchors(&functions);
    let jsx_react_shape_anchors = jsx_react_shape_anchors(path, source);
    SourceEvidenceProfile {
        path: path.to_string(),
        fingerprint,
        function_axis_anchors,
        jsx_react_shape_anchors,
    }
}

#[must_use]
pub fn source_evidence_idf<'a>(
    profiles: impl IntoIterator<Item = &'a SourceEvidenceProfile>,
) -> SourceEvidenceIdf {
    let mut n = 0usize;
    let mut df = BTreeMap::<String, usize>::new();
    for profile in profiles {
        n += 1;
        for anchor in &profile.fingerprint.string_anchors {
            *df.entry(anchor.clone()).or_insert(0) += 1;
        }
    }
    let corpus_size = n.max(1) as f64;
    let string_anchors = df
        .iter()
        .map(|(anchor, count)| (anchor.clone(), (corpus_size / (*count as f64)).ln()))
        .collect();
    SourceEvidenceIdf {
        string_anchors,
        string_anchor_document_frequency: df,
    }
}

#[must_use]
pub fn score_source_evidence(
    subject: &SourceEvidenceProfile,
    reference: &SourceEvidenceProfile,
    idf: &SourceEvidenceIdf,
) -> SourceEvidenceScore {
    let hash_match = !subject
        .fingerprint
        .normalized_source_hashes
        .is_disjoint(&reference.fingerprint.normalized_source_hashes);
    let weighted_string_anchor = weighted_overlap(
        &subject.fingerprint.string_anchors,
        &reference.fingerprint.string_anchors,
        &idf.string_anchors,
    );
    let normalized_string_anchor = normalized_weighted_overlap(
        &subject.fingerprint.string_anchors,
        &reference.fingerprint.string_anchors,
        &idf.string_anchors,
        weighted_string_anchor,
    );
    let unique_string_anchor_overlap = subject
        .fingerprint
        .string_anchors
        .intersection(&reference.fingerprint.string_anchors)
        .filter(|anchor| {
            idf.string_anchor_document_frequency
                .get(*anchor)
                .copied()
                .unwrap_or(usize::MAX)
                == 1
        })
        .count();
    let function_axis_overlap = subject
        .function_axis_anchors
        .intersection(&reference.function_axis_anchors)
        .count();
    let function_axis_jaccard = set_jaccard(
        &subject.function_axis_anchors,
        &reference.function_axis_anchors,
    );
    let function_axis_containment = set_containment(
        &subject.function_axis_anchors,
        &reference.function_axis_anchors,
    );
    let jsx_react_shape_overlap = subject
        .jsx_react_shape_anchors
        .intersection(&reference.jsx_react_shape_anchors)
        .count();
    let jsx_react_shape_jaccard = set_jaccard(
        &subject.jsx_react_shape_anchors,
        &reference.jsx_react_shape_anchors,
    );
    SourceEvidenceScore {
        hash_match,
        function_axis_overlap,
        function_axis_jaccard,
        function_axis_containment,
        weighted_string_anchor,
        normalized_string_anchor,
        unique_string_anchor_overlap,
        jsx_react_shape_overlap,
        jsx_react_shape_jaccard,
    }
}

#[must_use]
pub fn function_axis_anchors(functions: &[FunctionFingerprint]) -> BTreeSet<String> {
    let mut anchors = BTreeSet::new();
    for function in functions {
        record_axis_hashes(
            &mut anchors,
            NormalizationPassId::Primary,
            function.param_count,
            function.statement_count,
            &function.primary,
        );
        for alternate in &function.alternates {
            record_axis_hashes(
                &mut anchors,
                alternate.pass,
                function.param_count,
                alternate.statement_count,
                &alternate.axes,
            );
        }
    }
    anchors
}

fn record_axis_hashes(
    anchors: &mut BTreeSet<String>,
    pass: NormalizationPassId,
    param_count: u32,
    statement_count: u32,
    hashes: &AxisHashes,
) {
    for axis in [
        AxisKind::Ast,
        AxisKind::Cfg,
        AxisKind::ReturnPattern,
        AxisKind::EffectPattern,
        AxisKind::LiteralAnchor,
        AxisKind::AccessPattern,
        AxisKind::StructuralAnchor,
        AxisKind::LiteralShape,
        AxisKind::AccessShape,
        AxisKind::ExpressionShape,
        AxisKind::CalleeSet,
        AxisKind::BindingPattern,
        AxisKind::ThrowSet,
    ] {
        let Some(hash) = hashes.get(axis) else {
            continue;
        };
        anchors.insert(format!(
            "fn-axis:{}:{}:{}:{param_count}:{statement_count}:{hash:016x}",
            pass.as_str(),
            axis.as_str(),
            axis_strength(axis)
        ));
    }
}

const fn axis_strength(axis: AxisKind) -> &'static str {
    match axis {
        AxisKind::Ast | AxisKind::LiteralAnchor | AxisKind::StructuralAnchor => "strong",
        AxisKind::Cfg
        | AxisKind::ReturnPattern
        | AxisKind::EffectPattern
        | AxisKind::LiteralShape
        | AxisKind::AccessShape
        | AxisKind::CalleeSet
        | AxisKind::BindingPattern
        | AxisKind::ThrowSet
        | AxisKind::AccessPattern
        | AxisKind::NormalizedCfg
        | AxisKind::ExpressionShape => "shape",
    }
}

#[must_use]
pub fn jsx_react_shape_anchors(path: &str, source: &str) -> BTreeSet<String> {
    let allocator = Allocator::default();
    for source_type in source_type_candidates(Some(Path::new(path)), ParseGoal::TypeScript) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.panicked || !parsed.errors.is_empty() {
            continue;
        }
        let mut visitor = JsxReactShapeVisitor::default();
        visitor.visit_program(&parsed.program);
        return visitor.anchors;
    }
    BTreeSet::new()
}

#[derive(Default)]
struct JsxReactShapeVisitor {
    anchors: BTreeSet<String>,
}

impl<'a> Visit<'a> for JsxReactShapeVisitor {
    fn visit_jsx_opening_element(&mut self, element: &JSXOpeningElement<'a>) {
        let tag = jsx_element_name(&element.name);
        self.record_tag(&tag);
        for attribute in &element.attributes {
            if let JSXAttributeItem::Attribute(attribute) = attribute
                && let Some(name) = jsx_attribute_name(&attribute.name)
            {
                self.record_attr(&name);
                self.anchors.insert(format!("jsx-prop:{tag}:{name}"));
            }
        }
        if element.self_closing {
            self.anchors.insert(format!("jsx-self:{tag}"));
        }
        walk_jsx_opening_element(self, element);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Some(callee) = callee_name(&call.callee) {
            if matches!(
                callee.as_str(),
                "jsx" | "jsxs" | "jsxDEV" | "_jsx" | "_jsxs"
            ) {
                if let Some(tag) = first_string_like_argument(call) {
                    self.record_tag(&tag);
                    self.anchors.insert(format!("jsx-runtime:{tag}"));
                }
            } else if callee == "React.createElement" || callee == "createElement" {
                if let Some(tag) = first_string_like_argument(call) {
                    self.record_tag(&tag);
                    self.anchors.insert(format!("jsx-create:{tag}"));
                }
            } else if is_react_hook_name(&callee) {
                self.anchors.insert(format!("react-hook:{callee}"));
            }
        }
        walk_call_expression(self, call);
    }
}

impl JsxReactShapeVisitor {
    fn record_tag(&mut self, tag: &str) {
        let normalized = normalize_hint_text(tag);
        if normalized.len() >= 2 {
            self.anchors.insert(format!("jsx-tag:{tag}"));
        }
    }

    fn record_attr(&mut self, attr: &str) {
        let normalized = normalize_hint_text(attr);
        if normalized.len() >= 2 {
            self.anchors.insert(format!("jsx-attr:{attr}"));
        }
    }
}

fn jsx_element_name(name: &JSXElementName<'_>) -> String {
    match name {
        JSXElementName::Identifier(identifier) => identifier.name.to_string(),
        JSXElementName::IdentifierReference(identifier) => identifier.name.to_string(),
        JSXElementName::NamespacedName(namespaced) => {
            format!("{}:{}", namespaced.namespace.name, namespaced.property.name)
        }
        JSXElementName::MemberExpression(member) => jsx_member_expression_name(member),
        JSXElementName::ThisExpression(_) => "this".to_string(),
    }
}

fn jsx_member_expression_name(member: &JSXMemberExpression<'_>) -> String {
    format!(
        "{}.{}",
        jsx_member_object_name(&member.object),
        member.property.name
    )
}

fn jsx_member_object_name(object: &JSXMemberExpressionObject<'_>) -> String {
    match object {
        JSXMemberExpressionObject::IdentifierReference(identifier) => identifier.name.to_string(),
        JSXMemberExpressionObject::MemberExpression(member) => jsx_member_expression_name(member),
        JSXMemberExpressionObject::ThisExpression(_) => "this".to_string(),
    }
}

fn jsx_attribute_name(name: &JSXAttributeName<'_>) -> Option<String> {
    match name {
        JSXAttributeName::Identifier(identifier) => Some(identifier.name.to_string()),
        JSXAttributeName::NamespacedName(namespaced) => Some(format!(
            "{}:{}",
            namespaced.namespace.name, namespaced.property.name
        )),
    }
}

fn callee_name(expression: &Expression<'_>) -> Option<String> {
    match expression {
        Expression::Identifier(identifier) => Some(identifier.name.to_string()),
        Expression::StaticMemberExpression(member) => {
            let object = callee_name(&member.object)?;
            Some(format!("{}.{}", object, member.property.name))
        }
        Expression::ParenthesizedExpression(parenthesized) => {
            callee_name(&parenthesized.expression)
        }
        _ => None,
    }
}

fn first_string_like_argument(call: &CallExpression<'_>) -> Option<String> {
    let first = call.arguments.first()?;
    match first {
        Argument::StringLiteral(literal) => Some(literal.value.to_string()),
        Argument::Identifier(identifier) => Some(identifier.name.to_string()),
        Argument::StaticMemberExpression(member) => Some(format!(
            "{}.{}",
            callee_name(&member.object)?,
            member.property.name
        )),
        _ => None,
    }
}

fn is_react_hook_name(name: &str) -> bool {
    matches!(
        name,
        "useState"
            | "useEffect"
            | "useMemo"
            | "useCallback"
            | "useRef"
            | "useReducer"
            | "useContext"
            | "useLayoutEffect"
            | "useImperativeHandle"
            | "useSyncExternalStore"
            | "React.useState"
            | "React.useEffect"
            | "React.useMemo"
            | "React.useCallback"
            | "React.useRef"
            | "React.useReducer"
            | "React.useContext"
            | "React.useLayoutEffect"
            | "React.useImperativeHandle"
            | "React.useSyncExternalStore"
    ) || name
        .strip_prefix("use")
        .is_some_and(|suffix| suffix.chars().next().is_some_and(char::is_uppercase))
}

fn weighted_overlap(
    left: &BTreeSet<String>,
    right: &BTreeSet<String>,
    weights: &BTreeMap<String, f64>,
) -> f64 {
    left.intersection(right)
        .map(|anchor| weights.get(anchor).copied().unwrap_or(0.0))
        .sum()
}

fn weighted_mass(anchors: &BTreeSet<String>, weights: &BTreeMap<String, f64>) -> f64 {
    anchors
        .iter()
        .map(|anchor| weights.get(anchor).copied().unwrap_or(0.0))
        .sum()
}

fn normalized_weighted_overlap(
    left: &BTreeSet<String>,
    right: &BTreeSet<String>,
    weights: &BTreeMap<String, f64>,
    weighted_overlap: f64,
) -> f64 {
    let left_mass = weighted_mass(left, weights);
    let right_mass = weighted_mass(right, weights);
    if left_mass <= f64::EPSILON || right_mass <= f64::EPSILON {
        0.0
    } else {
        weighted_overlap / (left_mass * right_mass).sqrt()
    }
}

fn set_jaccard(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    let union = left.union(right).count();
    if union == 0 {
        0.0
    } else {
        left.intersection(right).count() as f64 / union as f64
    }
}

fn set_containment(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    let smaller = left.len().min(right.len());
    if smaller == 0 {
        0.0
    } else {
        left.intersection(right).count() as f64 / smaller as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(path: &str, source: &str) -> SourceEvidenceProfile {
        match build_source_evidence_profile(path, source) {
            Ok(profile) => profile,
            Err(error) => panic!("profile failed: {error}"),
        }
    }

    #[test]
    fn source_evidence_scores_unique_strings_and_function_axes() {
        let reference = profile(
            "panel.ts",
            "export function show(value: string) { return value.trim() + 'unique-panel-token'; }",
        );
        let subject = profile(
            "bundle.js",
            "export function a(b) { return b.trim() + 'unique-panel-token'; }",
        );
        let idf = source_evidence_idf([&reference]);
        let score = score_source_evidence(&subject, &reference, &idf);
        assert!(score.unique_string_anchor_overlap >= 1);
        assert!(score.function_axis_overlap >= 1);
        assert!(score.has_evidence());
    }

    #[test]
    fn jsx_react_shape_extracts_raw_jsx_and_runtime_calls() {
        let raw = jsx_react_shape_anchors(
            "view.tsx",
            "export function View(){ useEffect(() => {}, []); return <Panel title=\"x\" />; }",
        );
        assert!(raw.contains("jsx-tag:Panel"));
        assert!(raw.contains("jsx-attr:title"));
        assert!(raw.contains("react-hook:useEffect"));

        let lowered = jsx_react_shape_anchors(
            "view.js",
            "export function View(){ return _jsx('Panel', { title: 'x' }); }",
        );
        assert!(lowered.contains("jsx-runtime:Panel"));
        assert!(lowered.contains("jsx-tag:Panel"));
    }
}
