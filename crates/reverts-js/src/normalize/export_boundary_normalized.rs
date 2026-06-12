use oxc_allocator::Allocator;
use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Program, Statement};
use reverts_ir::NormalizationPassId;

use super::NormalizationPass;

pub struct ExportBoundaryNormalized;

impl NormalizationPass for ExportBoundaryNormalized {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ExportBoundaryNormalized
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut new_body = oxc_allocator::Vec::with_capacity_in(program.body.len(), alloc);
        for stmt in program.body.drain(..) {
            if let Some(replacement) = strip_export(stmt) {
                new_body.push(replacement);
            }
        }
        program.body = new_body;
    }
}

fn strip_export<'a>(stmt: Statement<'a>) -> Option<Statement<'a>> {
    match stmt {
        Statement::ExportNamedDeclaration(boxed) => {
            let export = boxed.unbox();
            match export.declaration {
                Some(Declaration::FunctionDeclaration(f)) => {
                    Some(Statement::FunctionDeclaration(f))
                }
                Some(Declaration::ClassDeclaration(c)) => Some(Statement::ClassDeclaration(c)),
                Some(Declaration::VariableDeclaration(v)) => {
                    Some(Statement::VariableDeclaration(v))
                }
                Some(_) => None,
                None => None,
            }
        }
        Statement::ExportDefaultDeclaration(boxed) => {
            let export = boxed.unbox();
            match export.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(f) if f.id.is_some() => {
                    Some(Statement::FunctionDeclaration(f))
                }
                ExportDefaultDeclarationKind::ClassDeclaration(c) if c.id.is_some() => {
                    Some(Statement::ClassDeclaration(c))
                }
                _ => None,
            }
        }
        Statement::ExportAllDeclaration(_) => None,
        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn export_keyword_is_stripped_from_function_decl() {
        let src = "export function f(a) { return a; }";
        let out = apply_to_source(&ExportBoundaryNormalized, src).expect("parses");
        assert!(!out.contains("export"), "got: {out}");
        assert!(out.contains("function f"), "got: {out}");
    }

    #[test]
    fn pure_reexport_drops_out_safely() {
        let src = "export { foo } from './bar';\nfunction g() {}";
        let out =
            apply_to_source(&ExportBoundaryNormalized, src).expect("normalize should succeed");
        assert!(out.contains("function g"));
    }

    #[test]
    fn export_default_named_function_is_stripped() {
        let src = "export default function f(a) { return a; }";
        let out = apply_to_source(&ExportBoundaryNormalized, src).expect("parses");
        assert!(!out.contains("export"), "got: {out}");
        assert!(out.contains("function f"), "got: {out}");
    }
}
