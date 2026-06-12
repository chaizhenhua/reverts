use oxc_ast::Visit;
use oxc_ast::ast::Program;

/// Returns `true` if the program may shadow the global identifier
/// `name` — i.e. there is a lexical or var binding with that name
/// anywhere in the file, OR a `with` statement is present (which can
/// dynamically introduce one). Callers that depend on a *spec-safe*
/// rewrite of `name`-as-a-global must bail out when this returns
/// `true`.
///
/// Shared by guarded-rewrite passes that need to prove a builtin like
/// `undefined`, `Boolean`, `Number`, etc. resolves to the real global
/// in this program.
#[must_use]
pub fn program_can_shadow(program: &Program<'_>, name: &str) -> bool {
    struct Checker<'n> {
        name: &'n str,
        found: bool,
    }
    impl<'a, 'n> Visit<'a> for Checker<'n> {
        fn visit_binding_identifier(&mut self, b: &oxc_ast::ast::BindingIdentifier<'a>) {
            if b.name.as_str() == self.name {
                self.found = true;
            }
        }
        fn visit_with_statement(&mut self, _: &oxc_ast::ast::WithStatement<'a>) {
            self.found = true;
        }
    }
    let mut c = Checker { name, found: false };
    c.visit_program(program);
    c.found
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn check(src: &str, name: &str) -> bool {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        program_can_shadow(&parsed.program, name)
    }

    #[test]
    fn detects_let_binding_shadow() {
        assert!(check("let undefined = 1;", "undefined"));
    }

    #[test]
    fn detects_param_shadow() {
        assert!(check("function f(undefined) {}", "undefined"));
    }

    #[test]
    fn detects_with_statement() {
        assert!(check("function f(o) { with (o) {} }", "undefined"));
    }

    #[test]
    fn returns_false_when_no_shadow() {
        assert!(!check("function f(x) { return x; }", "undefined"));
    }

    #[test]
    fn matches_specific_name_not_others() {
        assert!(!check("let foo = 1;", "Boolean"));
        assert!(check("let Boolean = 1;", "Boolean"));
    }
}
