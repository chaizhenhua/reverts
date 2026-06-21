mod classify;
mod commonjs_exports;
mod errors;
mod facts;
mod format;
mod format_module_items;
mod generated_statements;
mod identifier;
mod identifier_inventory;
mod import_coalesce;
mod lazy;
mod local_named_exports;
mod lowering;
mod namespace_flatten;
mod namespace_split;
pub mod normalize;
mod parse;
mod recover;
mod rename_apply;
mod rename_hints;
mod type_annotations;

pub use classify::{
    DeclarationCallability, ImportUsageScope, classify_import_usage_scope,
    classify_top_level_bindings, verify_only_immediate_call_references,
};
pub use commonjs_exports::{
    commonjs_create_binding_export_member, commonjs_export_property_name,
    commonjs_module_exports_target, expression_is_commonjs_exports_object, module_export_name,
    object_define_property_export_member, static_or_private_property_key_name_ref,
    static_property_key_name, static_property_key_name_ref,
};
pub use format::{format_source_minified, format_source_pretty, normalize_source_for_pipeline};
pub use format_module_items::{
    FormatSourceRequest, format_source_with_module_items,
    format_source_with_module_items_and_renames,
    format_source_with_module_items_and_renames_with_report,
    format_source_with_module_items_request,
};
pub use lowering::CompilerLowering;
pub use rename_apply::FunctionParamRename;

pub use errors::{JsError, ParseError, ParseGoal, Result, parse_error_message};
pub use facts::{
    IdentifierReadFact, LazyValueSubSnippet, PathBuilderCallFact, SourceLocationRewriteFact,
    StatementSpanFact, StaticModuleSpecifierFact, StaticTemplateLiteralFact, StringLiteralFact,
    TopLevelStatementFact, TopLevelStatementKind, collect_dead_top_level_bindings,
    collect_exported_top_level_bindings, collect_file_url_source_location_rewrites,
    collect_identifier_read_facts, collect_path_builder_calls, collect_static_module_specifiers,
    collect_static_resource_specifiers, collect_static_template_literals, collect_string_literals,
    collect_top_level_statement_facts, collect_void_zero_expression_statements,
    lazy_value_sub_snippets,
};
pub use identifier::{
    is_ascii_identifier_continue, is_ascii_identifier_start, is_generated_placeholder_identifier,
    is_identifier_like_ascii, is_identifier_part, is_identifier_start, is_js_keyword,
    is_minified_identifier, is_valid_static_member_property_name, read_identifier_at,
    read_identifier_with_end_at, read_quoted_string_at, sanitize_identifier, skip_block_comment,
    skip_line_comment,
};
pub use identifier_inventory::{IdentifierInventoryStats, collect_identifier_inventory};
pub use lazy::{
    LazyBodyClassification, classify_lazy_module_body, extract_lazy_module_eager_value,
    extract_lazy_module_eager_value_with_safe_deps,
};
pub(crate) use local_named_exports::module_export_name_text;
pub use parse::{parse_options_for, parse_source, source_type_candidates, source_type_for_parse};
pub use type_annotations::{
    GeneratedTypeAnnotation, GeneratedTypeKind, TypeCoverageStats,
    apply_import_member_type_queries_to_program, apply_type_annotations_to_program,
    collect_top_level_literal_type_annotations, collect_type_coverage_stats,
};

use oxc_ast::ast::Expression;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedImport {
    pub namespace: String,
    pub specifier: String,
    pub attributes: Vec<(String, String)>,
}

impl GeneratedImport {
    #[must_use]
    pub fn new(namespace: impl Into<String>, specifier: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            specifier: specifier.into(),
            attributes: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.push((key.into(), value.into()));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedExport {
    pub binding: String,
}

impl GeneratedExport {
    #[must_use]
    pub fn new(binding: impl Into<String>) -> Self {
        Self {
            binding: binding.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedRename {
    pub original: String,
    pub renamed: String,
    pub scope: GeneratedRenameScope,
    /// When true, also rewrite the binding's module import/export *wire* name
    /// (the `imported`/`exported` specifier name), collapsing the alias that the
    /// local rename leaves behind (`import { Cb as parseDocument }` →
    /// `import { parseDocument }`). The planner sets this only for bindings it
    /// proved safe to rename project-wide; the wire pass still only touches a
    /// specifier whose local was actually renamed.
    pub wire: bool,
}

impl GeneratedRename {
    #[must_use]
    pub fn new(original: impl Into<String>, renamed: impl Into<String>) -> Self {
        Self {
            original: original.into(),
            renamed: renamed.into(),
            scope: GeneratedRenameScope::Module,
            wire: false,
        }
    }

    #[must_use]
    pub fn new_all_scopes(original: impl Into<String>, renamed: impl Into<String>) -> Self {
        Self {
            original: original.into(),
            renamed: renamed.into(),
            scope: GeneratedRenameScope::All,
            wire: false,
        }
    }

    #[must_use]
    pub fn new_binding_index(
        original: impl Into<String>,
        renamed: impl Into<String>,
        binding_index: u32,
    ) -> Self {
        Self {
            original: original.into(),
            renamed: renamed.into(),
            scope: GeneratedRenameScope::BindingIndex(binding_index),
            wire: false,
        }
    }

    /// Mark this rename to also rewrite the module import/export wire name.
    #[must_use]
    pub fn with_wire(mut self) -> Self {
        self.wire = true;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratedRenameScope {
    /// Rename only the root/module-scope binding with this original name.
    Module,
    /// Rename every safely-resolved binding with this original name, including
    /// function parameters, catch bindings, and nested locals.
    All,
    /// Rename one binding occurrence for this original name. The index is the
    /// 1-based AST binding occurrence ordinal among bindings with that name in
    /// the emitted file.
    BindingIndex(u32),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadabilityReport {
    pub entries: Vec<String>,
}

impl ReadabilityReport {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn push(&mut self, entry: impl Into<String>) {
        self.entries.push(entry.into());
    }
}

pub(crate) fn expression_identifier<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::Identifier(identifier) => Some(identifier.name.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
