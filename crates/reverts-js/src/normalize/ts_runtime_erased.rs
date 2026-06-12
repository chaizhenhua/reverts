use oxc_allocator::Allocator;
use oxc_ast::ast::{Declaration, Program, Statement};
use reverts_ir::NormalizationPassId;

use super::NormalizationPass;

pub struct TsRuntimeErased;

impl NormalizationPass for TsRuntimeErased {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::TsRuntimeErased
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, _alloc: &'a Allocator, program: &mut Program<'a>) {
        program.body.retain(|stmt| !is_ts_only_top_level(stmt));
    }
}

fn is_ts_only_top_level(stmt: &Statement<'_>) -> bool {
    if matches!(
        stmt,
        Statement::TSTypeAliasDeclaration(_)
            | Statement::TSInterfaceDeclaration(_)
            | Statement::TSEnumDeclaration(_)
            | Statement::TSModuleDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_)
            | Statement::TSExportAssignment(_)
            | Statement::TSNamespaceExportDeclaration(_)
    ) {
        return true;
    }
    if let Statement::ExportNamedDeclaration(export) = stmt
        && matches!(
            &export.declaration,
            Some(
                Declaration::TSTypeAliasDeclaration(_)
                    | Declaration::TSInterfaceDeclaration(_)
                    | Declaration::TSEnumDeclaration(_)
                    | Declaration::TSModuleDeclaration(_)
            )
        )
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn ts_runtime_erased_drops_interface_and_type_alias() {
        let src = "interface Foo { a: number }\ntype B = string;\nexport function f(x: number): number { return x + 1; }";
        let out = apply_to_source(&TsRuntimeErased, src).expect("source must parse");
        assert!(!out.contains("interface"), "got: {out}");
        assert!(!out.contains("type B"), "got: {out}");
        assert!(out.contains("function f"), "got: {out}");
    }

    #[test]
    fn ts_runtime_erased_is_idempotent_on_plain_js() {
        let src = "function add(a, b) { return a + b; }\n";
        let first = apply_to_source(&TsRuntimeErased, src).expect("first pass should succeed");
        let second = apply_to_source(&TsRuntimeErased, &first).expect("second pass should succeed");
        assert_eq!(first, second);
    }
}
