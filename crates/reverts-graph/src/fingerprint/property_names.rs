//! Property / method / member-access name collector.
//!
//! Bundlers (minifiers) cannot rename **property names** — class method
//! names, object-literal keys, and `obj.prop` member accesses — because
//! external code may call them with the original name. Names like
//! `componentDidMount`, `processInput`, `fromSSEResponse` survive
//! minification verbatim and form a strong identity signal for the
//! containing module.
//!
//! This collector walks an OXC-parsed program and returns the FNV-1a
//! hash of every distinct non-trivial property/method/member name it
//! finds (>= 3 chars, skipping `length` / `name` / `value` style
//! built-in noise that appears in nearly every module).

use std::collections::BTreeSet;

use oxc_allocator::Allocator;
use oxc_ast::Visit;
use oxc_ast::ast::{
    Class, ClassElement, ComputedMemberExpression, Expression, MethodDefinition, ObjectExpression,
    ObjectPropertyKind, PropertyKey, StaticMemberExpression,
};
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::parse_options_for;
use reverts_ir::hash::fnv1a;

const NOISE_NAMES: &[&str] = &[
    "length",
    "name",
    "value",
    "key",
    "prototype",
    "constructor",
    "toString",
    "valueOf",
    "hasOwnProperty",
    "default",
    "type",
];
const MIN_NAME_LEN: usize = 3;

#[must_use]
pub fn extract_property_names(source: &str) -> BTreeSet<u64> {
    let alloc = Allocator::default();
    let source_type = SourceType::default().with_typescript(true).with_jsx(true);
    let parsed = Parser::new(&alloc, source, source_type)
        .with_options(parse_options_for(source_type))
        .parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return BTreeSet::new();
    }
    let mut collector = Collector {
        out: BTreeSet::new(),
    };
    collector.visit_program(&parsed.program);
    collector.out
}

fn record(set: &mut BTreeSet<u64>, name: &str) {
    if name.len() < MIN_NAME_LEN {
        return;
    }
    if NOISE_NAMES.contains(&name) {
        return;
    }
    set.insert(fnv1a(name.as_bytes()));
}

struct Collector {
    out: BTreeSet<u64>,
}

impl<'a> Visit<'a> for Collector {
    fn visit_object_expression(&mut self, obj: &ObjectExpression<'a>) {
        for prop in &obj.properties {
            if let ObjectPropertyKind::ObjectProperty(p) = prop
                && let Some(name) = property_key_name(&p.key)
            {
                record(&mut self.out, name);
            }
        }
        oxc_ast::visit::walk::walk_object_expression(self, obj);
    }

    fn visit_static_member_expression(&mut self, member: &StaticMemberExpression<'a>) {
        record(&mut self.out, member.property.name.as_str());
        oxc_ast::visit::walk::walk_static_member_expression(self, member);
    }

    fn visit_computed_member_expression(&mut self, member: &ComputedMemberExpression<'a>) {
        if let Expression::StringLiteral(s) = &member.expression {
            record(&mut self.out, s.value.as_str());
        }
        oxc_ast::visit::walk::walk_computed_member_expression(self, member);
    }

    fn visit_class(&mut self, class: &Class<'a>) {
        for elem in &class.body.body {
            match elem {
                ClassElement::MethodDefinition(m) => visit_method(&mut self.out, m),
                ClassElement::PropertyDefinition(p) => {
                    if let Some(name) = property_key_name(&p.key) {
                        record(&mut self.out, name);
                    }
                }
                ClassElement::AccessorProperty(a) => {
                    if let Some(name) = property_key_name(&a.key) {
                        record(&mut self.out, name);
                    }
                }
                _ => {}
            }
        }
        oxc_ast::visit::walk::walk_class(self, class);
    }
}

fn visit_method(set: &mut BTreeSet<u64>, m: &MethodDefinition<'_>) {
    if let Some(name) = property_key_name(&m.key) {
        record(set, name);
    }
}

fn property_key_name<'a>(key: &'a PropertyKey<'a>) -> Option<&'a str> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.as_str()),
        PropertyKey::PrivateIdentifier(id) => Some(id.name.as_str()),
        PropertyKey::StringLiteral(s) => Some(s.value.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(source: &str) -> Vec<String> {
        let _ = source;
        // hashes only — for tests we compare on inputs that produce
        // known hashes.
        Vec::new()
    }

    #[test]
    fn class_methods_and_static_member_accesses_are_collected() {
        let src = r#"
            class Widget {
                componentDidMount() { this.foo.processInput(); }
                handleSetRawMode() { return 1; }
            }
            const a = { fromSSEResponse: () => 1 };
            a.takeStreamEvents();
        "#;
        let set = extract_property_names(src);
        assert!(set.contains(&fnv1a(b"componentDidMount")));
        assert!(set.contains(&fnv1a(b"handleSetRawMode")));
        assert!(set.contains(&fnv1a(b"fromSSEResponse")));
        assert!(set.contains(&fnv1a(b"takeStreamEvents")));
        assert!(set.contains(&fnv1a(b"processInput")));
        assert!(!set.contains(&fnv1a(b"length")));
        assert!(!set.contains(&fnv1a(b"constructor")));
        let _ = names("");
    }

    #[test]
    fn computed_string_member_accesses_are_collected() {
        let src = r#"obj["resetBackoff"]();"#;
        let set = extract_property_names(src);
        assert!(set.contains(&fnv1a(b"resetBackoff")));
    }

    #[test]
    fn short_names_are_skipped() {
        let src = r#"a.b.c.d();"#;
        let set = extract_property_names(src);
        assert!(set.is_empty());
    }
}
