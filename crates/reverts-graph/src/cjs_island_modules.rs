//! Structural recognition of inlined CommonJS module units in the eager entry
//! island.
//!
//! A scope-hoisting bundler (esbuild's `__esm`/`__commonJS`) inlines a whole
//! third-party module as a *memoized init thunk*: a zero-arg function that
//! populates a module-scope exports object exactly once, guarded by a flag.
//! After minification the recognizable SHAPE survives even though every name is
//! mangled, so we match the shape — never specific names, packages, or text:
//!
//! ```js
//! var EXPORTS = {};                  // module-scope, initialized to an empty object
//! var GUARD;                          // module-scope init-once flag (may be absent)
//! function INIT() {                   // zero-arg memoized initializer
//!   return GUARD || (GUARD = 1, /* …populate EXPORTS… */), EXPORTS;   // Form A
//! }
//! // or, equivalently:
//! function INIT() {
//!   if (GUARD) return EXPORTS;        // Form B
//!   GUARD = 1; /* …populate EXPORTS… */ return EXPORTS;
//! }
//! ```
//!
//! Each recognized unit is one inlined module: its `INIT` body is the module's
//! implementation and `EXPORTS` is what consumers read. Recovering these as
//! module units lets the package matcher work at MODULE granularity (robust,
//! fast) instead of fingerprinting every island binding individually.

use std::collections::BTreeSet;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, Function, Program, Statement};
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_syntax::operator::LogicalOperator;

/// One inlined CommonJS module unit recovered from the island, identified purely
/// by the structural init-thunk shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecognizedCjsModule {
    /// Name of the zero-arg memoized initializer function (`INIT`).
    pub init_fn: String,
    /// Name of the module-scope exports object (`EXPORTS`, a `var X = {}`).
    pub exports: String,
    /// Name of the init-once guard flag (`GUARD`), when one is present.
    pub guard: Option<String>,
    /// Byte span of the `INIT` function declaration in the source — the module's
    /// implementation, ready to fingerprint/match at module granularity.
    pub body_span: (u32, u32),
}

/// Recognize every inlined CommonJS module unit in `source` by its structural
/// shape. Returns an empty vec if the source does not parse.
#[must_use]
pub fn recognize_cjs_island_modules(source: &str) -> Vec<RecognizedCjsModule> {
    let allocator = Allocator::default();
    let source_type = SourceType::default().with_typescript(true);
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return Vec::new();
    }
    recognize_in_program(&parsed.program)
}

/// Recognize inlined CommonJS module units in an already-parsed program.
#[must_use]
pub fn recognize_in_program(program: &Program<'_>) -> Vec<RecognizedCjsModule> {
    // Anchor on the structural fact that distinguishes a module-exports object
    // from any other binding: a module-scope `var X = {}` initialized to an
    // EMPTY object literal. The init thunk must return one of these.
    let exports_objects = module_scope_empty_object_vars(program);
    if exports_objects.is_empty() {
        return Vec::new();
    }

    let mut modules = Vec::new();
    for statement in &program.body {
        let Statement::FunctionDeclaration(function) = statement else {
            continue;
        };
        let Some(module) = recognize_init_thunk(function, &exports_objects) else {
            continue;
        };
        modules.push(module);
    }
    modules
}

/// Names of module-scope `var X = {}` declarations (empty object literal init).
fn module_scope_empty_object_vars(program: &Program<'_>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for statement in &program.body {
        let Statement::VariableDeclaration(declaration) = statement else {
            continue;
        };
        for declarator in &declaration.declarations {
            let Some(Expression::ObjectExpression(object)) = &declarator.init else {
                continue;
            };
            if !object.properties.is_empty() {
                continue;
            }
            if let Some(name) = declarator.id.get_identifier() {
                names.insert(name.as_str().to_string());
            }
        }
    }
    names
}

/// Recognize a zero-arg memoized init thunk over one of `exports_objects`.
fn recognize_init_thunk(
    function: &Function<'_>,
    exports_objects: &BTreeSet<String>,
) -> Option<RecognizedCjsModule> {
    let id = function.id.as_ref()?;
    // Zero parameters: a CJS init thunk takes none.
    if !function.params.items.is_empty() || function.params.rest.is_some() {
        return None;
    }
    let body = function.body.as_ref()?;

    let (exports, guard) = recognize_form_a(&body.statements)
        .or_else(|| recognize_form_b(&body.statements))?;
    if !exports_objects.contains(&exports) {
        return None;
    }
    Some(RecognizedCjsModule {
        init_fn: id.name.to_string(),
        exports,
        guard,
        body_span: (function.span.start, function.span.end),
    })
}

/// Form A: a single `return GUARD || (GUARD = 1, …), EXPORTS;`. The argument is
/// a sequence whose last element is the exports identifier and whose first is a
/// logical-OR guarded on an identifier. Returns `(exports, guard)`.
fn recognize_form_a(statements: &[Statement<'_>]) -> Option<(String, Option<String>)> {
    let [Statement::ReturnStatement(ret)] = statements else {
        return None;
    };
    let Some(Expression::SequenceExpression(sequence)) = ret.argument.as_ref() else {
        return None;
    };
    let exports = identifier_name(sequence.expressions.last()?)?;
    // Some leading element must be `GUARD || (...)` — the memoization gate.
    let guard = sequence.expressions.iter().find_map(|expression| {
        let Expression::LogicalExpression(logical) = expression else {
            return None;
        };
        (logical.operator == LogicalOperator::Or)
            .then(|| identifier_name(&logical.left))
            .flatten()
    })?;
    Some((exports, Some(guard)))
}

/// Form B: a leading `if (GUARD) return EXPORTS;` memoization guard. Returns
/// `(exports, guard)`.
fn recognize_form_b(statements: &[Statement<'_>]) -> Option<(String, Option<String>)> {
    let Statement::IfStatement(if_statement) = statements.first()? else {
        return None;
    };
    let guard = identifier_name(&if_statement.test)?;
    let returned = return_statement_identifier(&if_statement.consequent)?;
    Some((returned, Some(guard)))
}

/// The identifier returned by a `return X;` statement (or a block whose first
/// statement is one) — used to read the exports identifier from a guard branch.
fn return_statement_identifier(statement: &Statement<'_>) -> Option<String> {
    let return_statement = match statement {
        Statement::ReturnStatement(ret) => ret,
        Statement::BlockStatement(block) => match block.body.first()? {
            Statement::ReturnStatement(ret) => ret,
            _ => return None,
        },
        _ => return None,
    };
    identifier_name(return_statement.argument.as_ref()?)
}

/// The name of a plain identifier-reference expression, else `None`.
fn identifier_name(expression: &Expression<'_>) -> Option<String> {
    match expression {
        Expression::Identifier(identifier) => Some(identifier.name.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_form_a_return_guard_or_sequence() {
        // esbuild minified `__esm` shape.
        let source = "var CH = {};\nvar pAe;\nfunction EOt() { return pAe || (pAe = 1, CH._x = 1), CH; }\n";
        let modules = recognize_cjs_island_modules(source);
        assert_eq!(modules.len(), 1, "{modules:?}");
        assert_eq!(modules[0].init_fn, "EOt");
        assert_eq!(modules[0].exports, "CH");
        assert_eq!(modules[0].guard.as_deref(), Some("pAe"));
    }

    #[test]
    fn recognizes_form_b_if_guard_return() {
        let source = "var UR = {};\nvar DAe;\nfunction BOt() { if (DAe) return UR; DAe = 1; UR.isCompatible = 1; return UR; }\n";
        let modules = recognize_cjs_island_modules(source);
        assert_eq!(modules.len(), 1, "{modules:?}");
        assert_eq!(modules[0].init_fn, "BOt");
        assert_eq!(modules[0].exports, "UR");
        assert_eq!(modules[0].guard.as_deref(), Some("DAe"));
    }

    #[test]
    fn recognizes_multiple_units_and_reports_spans() {
        let source = "var A = {};\nvar gA;\nfunction iA() { return gA || (gA = 1, A.v = 1), A; }\nvar B = {};\nvar gB;\nfunction iB() { return gB || (gB = 1, B.w = 2), B; }\n";
        let modules = recognize_cjs_island_modules(source);
        assert_eq!(modules.len(), 2, "{modules:?}");
        assert!(modules.iter().all(|m| m.body_span.1 > m.body_span.0));
    }

    #[test]
    fn rejects_non_empty_object_and_unrelated_functions() {
        // `var X = { a: 1 }` is not a module-exports object; a plain function is
        // not an init thunk; a guard returning a NON-exports identifier is out.
        let source = "var X = { a: 1 };\nvar Y = {};\nfunction f(a) { return a; }\nfunction g() { return Y2 || (Y2 = 1, 0), Y2; }\nfunction h() { return Y; }\n";
        let modules = recognize_cjs_island_modules(source);
        assert!(modules.is_empty(), "no init-thunk shape should match: {modules:?}");
    }

    #[test]
    fn rejects_init_thunk_over_non_exports_object() {
        // The guarded return exists, but `Z` is never a `var Z = {}` — so it is
        // not an inlined module-exports object and must not be recognized.
        let source = "var Q = {};\nvar gZ;\nfunction iZ() { return gZ || (gZ = 1, 0), Z; }\n";
        let modules = recognize_cjs_island_modules(source);
        assert!(modules.is_empty(), "{modules:?}");
    }

    #[test]
    fn empty_or_unparseable_source_yields_nothing() {
        assert!(recognize_cjs_island_modules("").is_empty());
        assert!(recognize_cjs_island_modules("var a = {};").is_empty());
    }
}
