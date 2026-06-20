//! Weak expression-shape hash used for source matching.
//!
//! This axis deliberately does **not** rewrite source. It records a bag of
//! identifier-blind expression shapes and canonicalises only conservative
//! commutative binary forms when both operands are simple/pure shapes. The
//! result is useful evidence for `a === b` versus `b === a`, but remains a weak
//! axis because JavaScript evaluation order is observable for arbitrary
//! expressions.

use std::collections::{BTreeMap, BTreeSet};

use oxc_ast::Visit;
use oxc_ast::ast::{
    Argument, BinaryOperator, BindingPattern, BindingPatternKind, Expression, FormalParameters,
    FunctionBody, ObjectPropertyKind, PropertyKind, Statement, VariableDeclarator,
};
use oxc_syntax::operator::UnaryOperator;
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};

#[must_use]
pub fn compute(params: &FormalParameters<'_>, body: &FunctionBody<'_>) -> Option<u64> {
    let mut counts = BTreeMap::<String, u32>::new();
    record_parameter_shapes(params, &mut counts);
    let mut visitor = Visitor {
        counts: &mut counts,
    };
    for stmt in &body.statements {
        visitor.visit_statement(stmt);
    }
    if counts.is_empty() {
        return None;
    }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"expr_shape|");
    for (shape, count) in counts {
        update_fnv1a(&mut hash, shape.as_bytes());
        update_fnv1a(&mut hash, b"=");
        update_fnv1a(&mut hash, count.to_string().as_bytes());
        update_fnv1a(&mut hash, b"|");
    }
    Some(hash)
}

struct Visitor<'c> {
    counts: &'c mut BTreeMap<String, u32>,
}

impl<'a> Visit<'a> for Visitor<'_> {
    fn visit_statement(&mut self, stmt: &Statement<'a>) {
        match stmt {
            Statement::ClassDeclaration(class) => {
                let shape = class_shape(&class.body.body);
                *self.counts.entry(shape).or_default() += 1;
            }
            Statement::SwitchStatement(switch) => {
                let mut cases = BTreeMap::<String, u32>::new();
                for case in &switch.cases {
                    let shape = case
                        .test
                        .as_ref()
                        .map_or_else(|| "default".to_string(), expression_shape);
                    *cases.entry(shape).or_default() += 1;
                }
                let token = cases
                    .into_iter()
                    .map(|(shape, count)| format!("{shape}={count}"))
                    .collect::<Vec<_>>()
                    .join(",");
                *self.counts.entry(format!("switch{{{token}}}")).or_default() += 1;
            }
            _ => {}
        }
        oxc_ast::visit::walk::walk_statement(self, stmt);
    }

    fn visit_variable_declarator(&mut self, declarator: &VariableDeclarator<'a>) {
        let shape = binding_pattern_shape(&declarator.id);
        *self
            .counts
            .entry(format!("local-bind:{shape}"))
            .or_default() += 1;
        oxc_ast::visit::walk::walk_variable_declarator(self, declarator);
    }

    fn visit_expression(&mut self, expr: &Expression<'a>) {
        if let Some(shape) = optional_equivalent_shape(expr) {
            *self.counts.entry(shape).or_default() += 1;
            return;
        }
        let shape = expression_shape(expr);
        *self.counts.entry(shape).or_default() += 1;
        oxc_ast::visit::walk::walk_expression(self, expr);
    }
}

fn expression_shape(expr: &Expression<'_>) -> String {
    use Expression as E;
    match expr {
        E::Identifier(_) => "id".to_string(),
        E::ThisExpression(_) => "this".to_string(),
        E::StringLiteral(_) => "lit:str".to_string(),
        E::NumericLiteral(_) => "lit:num".to_string(),
        E::BooleanLiteral(_) => "lit:bool".to_string(),
        E::NullLiteral(_) => "lit:null".to_string(),
        E::BigIntLiteral(_) => "lit:bigint".to_string(),
        E::RegExpLiteral(_) => "lit:regexp".to_string(),
        E::TemplateLiteral(t) => format!("tpl:{}:{}", t.quasis.len(), t.expressions.len()),
        E::UnaryExpression(u) => {
            if matches!(u.operator, UnaryOperator::LogicalNot)
                && matches!(&u.argument, E::NumericLiteral(n) if n.value == 0.0 || n.value == 1.0)
            {
                return "lit:bool".to_string();
            }
            format!("un:{:?}({})", u.operator, expression_shape(&u.argument))
        }
        E::BinaryExpression(b) => binary_shape(b.operator, &b.left, &b.right),
        E::LogicalExpression(l) => format!(
            "log:{:?}({},{})",
            l.operator,
            expression_shape(&l.left),
            expression_shape(&l.right)
        ),
        E::ConditionalExpression(c) => nullish_guard_shape(&c.test, &c.consequent, &c.alternate)
            .unwrap_or_else(|| {
                format!(
                    "cond({},{},{})",
                    expression_shape(&c.test),
                    expression_shape(&c.consequent),
                    expression_shape(&c.alternate)
                )
            }),
        E::CallExpression(c) => format!(
            "call:{}:{}",
            expression_shape(&c.callee),
            argument_shape_list(&c.arguments)
        ),
        E::NewExpression(n) => format!(
            "new:{}:{}",
            expression_shape(&n.callee),
            argument_shape_list(&n.arguments)
        ),
        E::StaticMemberExpression(m) => {
            format!("mem:s:{}:.{}", expression_shape(&m.object), m.property.name)
        }
        E::ComputedMemberExpression(m) => format!(
            "mem:c:{}:[{}]",
            expression_shape(&m.object),
            expression_shape(&m.expression)
        ),
        E::AssignmentExpression(a) => {
            format!("assign:{:?}({})", a.operator, expression_shape(&a.right))
        }
        E::ArrayExpression(a) => {
            let elements = a
                .elements
                .iter()
                .map(|element| {
                    element
                        .as_expression()
                        .map_or_else(|| "hole/spread".to_string(), expression_shape)
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("arr[{elements}]")
        }
        E::ObjectExpression(o) => object_shape(&o.properties),
        E::ArrowFunctionExpression(a) => {
            format!("arrow:{}:{}", a.params.items.len(), a.body.statements.len())
        }
        E::FunctionExpression(f) => format!(
            "fn:{}:{}",
            f.params.items.len(),
            f.body.as_ref().map_or(0, |body| body.statements.len())
        ),
        E::ClassExpression(c) => class_shape(&c.body.body),
        E::AwaitExpression(a) => format!("await({})", expression_shape(&a.argument)),
        E::YieldExpression(y) => y.argument.as_ref().map_or_else(
            || "yield".to_string(),
            |arg| format!("yield({})", expression_shape(arg)),
        ),
        E::ParenthesizedExpression(p) => expression_shape(&p.expression),
        E::SequenceExpression(s) => format!(
            "seq({})",
            s.expressions
                .iter()
                .map(expression_shape)
                .collect::<Vec<_>>()
                .join(",")
        ),
        E::ChainExpression(c) => chain_shape(&c.expression),
        _ => format!("other:{:?}", expr),
    }
}

fn optional_equivalent_shape(expr: &Expression<'_>) -> Option<String> {
    match expr {
        Expression::ChainExpression(chain) => Some(chain_shape(&chain.expression)),
        Expression::ConditionalExpression(conditional) => nullish_guard_shape(
            &conditional.test,
            &conditional.consequent,
            &conditional.alternate,
        ),
        Expression::ParenthesizedExpression(paren) => optional_equivalent_shape(&paren.expression),
        _ => None,
    }
}

fn record_parameter_shapes(params: &FormalParameters<'_>, counts: &mut BTreeMap<String, u32>) {
    for param in &params.items {
        let shape = binding_pattern_shape(&param.pattern);
        *counts.entry(format!("param-bind:{shape}")).or_default() += 1;
    }
    if let Some(rest) = &params.rest {
        let shape = binding_pattern_shape(&rest.argument);
        *counts.entry(format!("param-rest:{shape}")).or_default() += 1;
    }
}

fn binding_pattern_shape(pattern: &BindingPattern<'_>) -> String {
    match &pattern.kind {
        BindingPatternKind::BindingIdentifier(_) => "id".to_string(),
        BindingPatternKind::AssignmentPattern(assignment) => {
            format!("default({})", binding_pattern_shape(&assignment.left))
        }
        BindingPatternKind::ObjectPattern(object) => {
            let mut fields = Vec::new();
            for property in &object.properties {
                fields.push(binding_pattern_shape(&property.value));
            }
            fields.sort();
            if let Some(rest) = &object.rest {
                fields.push(format!("...{}", binding_pattern_shape(&rest.argument)));
            }
            format!("object{{{}}}", fields.join(","))
        }
        BindingPatternKind::ArrayPattern(array) => {
            let mut fields = Vec::new();
            for element in &array.elements {
                fields.push(
                    element
                        .as_ref()
                        .map_or_else(|| "hole".to_string(), binding_pattern_shape),
                );
            }
            if let Some(rest) = &array.rest {
                fields.push(format!("...{}", binding_pattern_shape(&rest.argument)));
            }
            format!("array[{}]", fields.join(","))
        }
    }
}

fn nullish_guard_shape(
    test: &Expression<'_>,
    consequent: &Expression<'_>,
    alternate: &Expression<'_>,
) -> Option<String> {
    let guarded = nullish_test_guard_shape(test)?;
    if !is_undefinedish(consequent) {
        return None;
    }
    let access = member_access_from_base_shape(alternate, guarded.as_str())?;
    Some(format!("opt:{access}"))
}

fn nullish_test_guard_shape(test: &Expression<'_>) -> Option<String> {
    use oxc_ast::ast::BinaryOperator;
    let Expression::BinaryExpression(binary) = test else {
        return None;
    };
    if !matches!(
        binary.operator,
        BinaryOperator::Equality | BinaryOperator::StrictEquality
    ) {
        return None;
    }
    if is_nullish_literal(&binary.left) {
        return Some(expression_shape(&binary.right));
    }
    if is_nullish_literal(&binary.right) {
        return Some(expression_shape(&binary.left));
    }
    None
}

fn is_nullish_literal(expr: &Expression<'_>) -> bool {
    matches!(expr, Expression::NullLiteral(_)) || is_undefinedish(expr)
}

fn is_undefinedish(expr: &Expression<'_>) -> bool {
    match expr {
        Expression::Identifier(identifier) => identifier.name == "undefined",
        Expression::UnaryExpression(unary) if matches!(unary.operator, UnaryOperator::Void) => true,
        Expression::ParenthesizedExpression(paren) => is_undefinedish(&paren.expression),
        _ => false,
    }
}

fn member_access_from_base_shape(expr: &Expression<'_>, base_shape: &str) -> Option<String> {
    match expr {
        Expression::StaticMemberExpression(member) => {
            let object = expression_shape(&member.object);
            if object == base_shape {
                Some(format!("mem:s:{base_shape}:.{}", member.property.name))
            } else {
                None
            }
        }
        Expression::ComputedMemberExpression(member) => {
            let object = expression_shape(&member.object);
            if object == base_shape {
                Some(format!(
                    "mem:c:{base_shape}:[{}]",
                    expression_shape(&member.expression)
                ))
            } else {
                None
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            member_access_from_base_shape(&paren.expression, base_shape)
        }
        _ => None,
    }
}

fn chain_shape(element: &oxc_ast::ast::ChainElement<'_>) -> String {
    use oxc_ast::ast::ChainElement;
    match element {
        ChainElement::StaticMemberExpression(member) => format!(
            "opt:mem:s:{}:.{}",
            expression_shape(&member.object),
            member.property.name
        ),
        ChainElement::ComputedMemberExpression(member) => format!(
            "opt:mem:c:{}:[{}]",
            expression_shape(&member.object),
            expression_shape(&member.expression)
        ),
        ChainElement::CallExpression(call) => format!(
            "opt:call:{}:{}",
            expression_shape(&call.callee),
            argument_shape_list(&call.arguments)
        ),
        ChainElement::TSNonNullExpression(inner) => expression_shape(&inner.expression),
        _ => format!("opt:other:{element:?}"),
    }
}

fn binary_shape(operator: BinaryOperator, left: &Expression<'_>, right: &Expression<'_>) -> String {
    let mut left_shape = expression_shape(left);
    let mut right_shape = expression_shape(right);
    if is_commutative(operator)
        && pure_commutative_operand_shape(left).is_some()
        && pure_commutative_operand_shape(right).is_some()
        && right_shape < left_shape
    {
        std::mem::swap(&mut left_shape, &mut right_shape);
    }
    format!("bin:{operator:?}({left_shape},{right_shape})")
}

fn is_commutative(operator: BinaryOperator) -> bool {
    matches!(
        operator,
        BinaryOperator::StrictEquality
            | BinaryOperator::StrictInequality
            | BinaryOperator::Equality
            | BinaryOperator::Inequality
            | BinaryOperator::Multiplication
            | BinaryOperator::BitwiseAnd
            | BinaryOperator::BitwiseOR
            | BinaryOperator::BitwiseXOR
    )
}

fn pure_commutative_operand_shape(expr: &Expression<'_>) -> Option<String> {
    use Expression as E;
    match expr {
        E::Identifier(_) => Some("id".to_string()),
        E::ThisExpression(_) => Some("this".to_string()),
        E::StringLiteral(_) => Some("lit:str".to_string()),
        E::NumericLiteral(_) => Some("lit:num".to_string()),
        E::BooleanLiteral(_) => Some("lit:bool".to_string()),
        E::NullLiteral(_) => Some("lit:null".to_string()),
        E::BigIntLiteral(_) => Some("lit:bigint".to_string()),
        E::ParenthesizedExpression(p) => pure_commutative_operand_shape(&p.expression),
        E::UnaryExpression(u) if matches!(u.operator, UnaryOperator::LogicalNot) => {
            pure_commutative_operand_shape(&u.argument).map(|inner| format!("un:not({inner})"))
        }
        _ => None,
    }
}

fn argument_shape_list<'a>(args: &oxc_allocator::Vec<'a, Argument<'a>>) -> String {
    args.iter()
        .map(|arg| {
            arg.as_expression()
                .map_or_else(|| "spread".to_string(), expression_shape)
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn object_shape(properties: &[ObjectPropertyKind<'_>]) -> String {
    let mut entries = BTreeSet::<String>::new();
    for property in properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            return "obj:opaque:spread/method".to_string();
        };
        if property.computed || property.kind != PropertyKind::Init {
            return "obj:opaque:computed/accessor".to_string();
        }
        let Some(key) = static_property_key(&property.key) else {
            return "obj:opaque:key".to_string();
        };
        if !entries.insert(format!("{key}:{}", expression_shape(&property.value))) {
            return "obj:opaque:duplicate".to_string();
        }
    }
    format!(
        "obj{{{}}}",
        entries.into_iter().collect::<Vec<_>>().join(",")
    )
}

fn class_shape(elements: &[oxc_ast::ast::ClassElement<'_>]) -> String {
    use oxc_ast::ast::ClassElement;
    let mut members = BTreeSet::<String>::new();
    for element in elements {
        match element {
            ClassElement::MethodDefinition(method) => {
                if method.computed {
                    return "class:opaque:computed".to_string();
                }
                let Some(key) = static_property_key(&method.key) else {
                    return "class:opaque:key".to_string();
                };
                let token = format!(
                    "m:{}:{}:{:?}:{}",
                    if method.r#static { "static" } else { "inst" },
                    key,
                    method.kind,
                    method.value.params.items.len()
                );
                if !members.insert(token) {
                    return "class:opaque:duplicate".to_string();
                }
            }
            ClassElement::PropertyDefinition(property) => {
                if property.computed {
                    return "class:opaque:computed-field".to_string();
                }
                let Some(key) = static_property_key(&property.key) else {
                    return "class:opaque:field-key".to_string();
                };
                let token = format!(
                    "f:{}:{}",
                    if property.r#static { "static" } else { "inst" },
                    key
                );
                if !members.insert(token) {
                    return "class:opaque:duplicate".to_string();
                }
            }
            ClassElement::StaticBlock(_) => return "class:opaque:static-block".to_string(),
            _ => return "class:opaque:other".to_string(),
        }
    }
    format!(
        "class{{{}}}",
        members.into_iter().collect::<Vec<_>>().join(",")
    )
}

fn static_property_key(key: &oxc_ast::ast::PropertyKey<'_>) -> Option<String> {
    match key {
        oxc_ast::ast::PropertyKey::StaticIdentifier(identifier) => {
            Some(identifier.name.to_string())
        }
        oxc_ast::ast::PropertyKey::StringLiteral(literal) => Some(literal.value.to_string()),
        oxc_ast::ast::PropertyKey::NumericLiteral(literal) => Some(literal.value.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn hash_first(src: &str) -> Option<u64> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(parsed.errors.is_empty(), "parse: {:?}", parsed.errors);
        let func = parsed
            .program
            .body
            .iter()
            .find_map(|s| {
                if let oxc_ast::ast::Statement::FunctionDeclaration(f) = s {
                    Some(f)
                } else {
                    None
                }
            })
            .expect("function");
        compute(&func.params, func.body.as_ref().expect("body"))
    }

    #[test]
    fn commutative_binary_operands_are_canonicalized_for_pure_shapes() {
        let left = hash_first("function f(a) { return a === 1; }");
        let right = hash_first("function f(a) { return 1 === a; }");
        assert_eq!(left, right);
    }

    #[test]
    fn non_commutative_binary_operands_keep_order() {
        let left = hash_first("function f(a) { return a - 1; }");
        let right = hash_first("function f(a) { return 1 - a; }");
        assert_ne!(left, right);
    }

    #[test]
    fn logical_short_circuit_operands_keep_order() {
        let left = hash_first("function f(a) { return a && true; }");
        let right = hash_first("function f(a) { return true && a; }");
        assert_ne!(left, right);
    }

    #[test]
    fn object_property_order_is_ignored_for_data_properties() {
        let left = hash_first("function f() { return { a: 1, b: 2 }; }");
        let right = hash_first("function f() { return { b: 2, a: 1 }; }");
        assert_eq!(left, right);
    }

    #[test]
    fn class_member_order_is_ignored_for_simple_members() {
        let left = hash_first("function f() { class C { a() {} b() {} } return C; }");
        let right = hash_first("function f() { class C { b() {} a() {} } return C; }");
        assert_eq!(left, right);
    }

    #[test]
    fn switch_case_order_is_available_as_weak_shape() {
        let left =
            hash_first("function f(x) { switch (x) { case 1: return a; case 2: return b; } }");
        let right =
            hash_first("function f(x) { switch (x) { case 2: return b; case 1: return a; } }");
        assert_eq!(left, right);
    }

    #[test]
    fn optional_chain_matches_simple_nullish_guard_shape() {
        let chain = hash_first("function f(a) { return a?.b; }");
        let guard = hash_first("function f(a) { return a == null ? undefined : a.b; }");
        assert_eq!(chain, guard);
    }

    #[test]
    fn default_rest_and_destructuring_patterns_contribute_weak_shape() {
        let shaped = hash_first(
            "function f({ a, b = 1, ...rest }, ...tail) { const [x, ...xs] = tail; return x; }",
        );
        let plain = hash_first("function f(a, tail) { const x = tail; return x; }");
        assert_ne!(shaped, plain);
    }
}
