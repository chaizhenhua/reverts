use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `ComputedToStaticMember` rewrites `obj["validIdent"]` into
/// `obj.validIdent` when the bracketed key is a string literal that
/// happens to be a valid JavaScript identifier and is not a reserved
/// word. The two forms are semantically identical: both look up the
/// same property on the same object.
///
/// Minifiers prefer the static-member form (one less byte per access),
/// while hand-written source sometimes uses the bracket form for
/// stylistic reasons (e.g. when the property name might shadow a
/// keyword, or to match dictionary-access semantics). The cascade
/// matcher's `access` and `callee_set` axes distinguish
/// `StaticMemberExpression` from `ComputedMemberExpression`: the
/// static form preserves the property name in the axis hash, the
/// computed form does not. Without this rewrite, identical code that
/// happens to spell the same access two different ways produces
/// divergent fingerprints.
pub struct ComputedToStaticMember;

impl NormalizationPass for ComputedToStaticMember {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ComputedToStaticMember
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Rewriter {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Rewriter<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Rewriter<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        // Descend bottom-up so nested expressions normalise first.
        walk_expression(self, expr);
        if !is_rewritable(expr) {
            return;
        }
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::ComputedMemberExpression(cme_box) = owned else {
            unreachable!()
        };
        let cme = cme_box.unbox();
        let key = match cme.expression {
            Expression::StringLiteral(s) => s,
            _ => unreachable!("is_rewritable already verified key is a StringLiteral"),
        };
        let ident_name = self.builder.identifier_name(SPAN, key.value.as_str());
        *expr = Expression::StaticMemberExpression(
            self.builder.alloc(self.builder.static_member_expression(
                SPAN,
                cme.object,
                ident_name,
                cme.optional,
            )),
        );
    }
}

fn is_rewritable(expr: &Expression<'_>) -> bool {
    let Expression::ComputedMemberExpression(cme) = expr else {
        return false;
    };
    let Expression::StringLiteral(s) = &cme.expression else {
        return false;
    };
    is_valid_identifier_name(s.value.as_str())
}

/// Whether `s` can be used as a `.identifier` (lexical identifier rules
/// per ECMA-262 §11.6.2 — `IdentifierStart` then `IdentifierPart*`),
/// AND is not a reserved word that would change parsing.
fn is_valid_identifier_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_id_start(first) {
        return false;
    }
    if !chars.all(is_id_part) {
        return false;
    }
    !is_reserved_word(s)
}

fn is_id_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphabetic() || unicode_id_start(c)
}

fn is_id_part(c: char) -> bool {
    is_id_start(c) || c.is_ascii_digit() || unicode_id_continue(c)
}

// Conservative bare-minimum unicode coverage: we don't include full
// Unicode ID_Start/ID_Continue tables, only ASCII + a handful of
// common non-ASCII letters. Property names that include non-ASCII
// characters are vanishingly rare in package code and would only
// cause us to miss an opportunity, never to produce invalid code.
fn unicode_id_start(_c: char) -> bool {
    false
}
fn unicode_id_continue(_c: char) -> bool {
    false
}

fn is_reserved_word(s: &str) -> bool {
    matches!(
        s,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
            | "let"
            | "static"
            | "enum"
            | "await"
            | "implements"
            | "interface"
            | "package"
            | "private"
            | "protected"
            | "public"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_bracket_string_to_dot_access() {
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(obj) { return obj["foo"]; }"#,
        )
        .expect("parse");
        assert!(out.contains("obj.foo"), "got: {out}");
        assert!(!out.contains("obj["), "got: {out}");
    }

    #[test]
    fn rewrites_chained_brackets() {
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(o) { return o["a"]["b"]["c"]; }"#,
        )
        .expect("parse");
        assert!(out.contains("o.a.b.c"), "got: {out}");
    }

    #[test]
    fn rewrites_bracket_call() {
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(o) { return o["push"](1); }"#,
        )
        .expect("parse");
        assert!(out.contains("o.push(1)"), "got: {out}");
    }

    #[test]
    fn leaves_non_identifier_keys_alone() {
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(o) { return o["with-dash"]; }"#,
        )
        .expect("parse");
        assert!(out.contains("\"with-dash\""), "got: {out}");
    }

    #[test]
    fn leaves_numeric_keys_alone() {
        let out = apply_to_source(&ComputedToStaticMember, r#"function f(o) { return o[0]; }"#)
            .expect("parse");
        assert!(out.contains("[0]"), "got: {out}");
    }

    #[test]
    fn leaves_reserved_words_alone() {
        // `obj.class` is technically legal post-ES5 but minifiers
        // historically left these as `obj["class"]` for compatibility,
        // and the safe move is to keep them bracketed.
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(o) { return o["class"]; }"#,
        )
        .expect("parse");
        assert!(out.contains("\"class\""), "got: {out}");
    }

    #[test]
    fn leaves_dynamic_key_alone() {
        let src = r#"function f(o, k) { return o[k]; }"#;
        let out = apply_to_source(&ComputedToStaticMember, src).expect("parse");
        assert!(out.contains("o[k]"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_static() {
        let src = "function f(o) { return o.foo.bar; }";
        let first = apply_to_source(&ComputedToStaticMember, src).expect("first");
        let second = apply_to_source(&ComputedToStaticMember, &first).expect("second");
        assert_eq!(first, second);
    }
}
