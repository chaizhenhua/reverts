use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::BindingName;

use crate::byte_lexer::skip_ws;
use crate::destructure_writes::split_top_level_properties;
use crate::identifiers::{declaration_keyword_at, keyword_at, parse_identifier};
use crate::{
    apply_text_edits, expand_line_removal_edits, identifier_read_facts_in_source,
    is_pure_initializer_expression, pure_class_expression, top_level_definitions_in_source,
    top_level_statement_spans,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeOrphanPrune {
    pub(crate) source: String,
    pub(crate) dropped_bindings: BTreeSet<BindingName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeBindingGraphStatement {
    start: usize,
    end: usize,
    definitions: BTreeSet<BindingName>,
    dependencies: BTreeSet<BindingName>,
    prunable: bool,
}

pub(crate) fn prune_orphan_runtime_bindings(
    source: &str,
    root_bindings: &BTreeSet<BindingName>,
) -> RuntimeOrphanPrune {
    let spans = top_level_statement_spans(source);
    if spans.is_empty() {
        return RuntimeOrphanPrune {
            source: source.to_string(),
            dropped_bindings: BTreeSet::new(),
        };
    }

    let mut all_definitions = BTreeSet::<BindingName>::new();
    for (start, end) in &spans {
        all_definitions.extend(top_level_definitions_in_source(&source[*start..*end]));
    }
    if all_definitions.is_empty() {
        return RuntimeOrphanPrune {
            source: source.to_string(),
            dropped_bindings: BTreeSet::new(),
        };
    }

    let mut statements = Vec::<RuntimeBindingGraphStatement>::new();
    let mut roots = root_bindings.clone();
    let mut statements_by_definition = BTreeMap::<BindingName, Vec<usize>>::new();

    for (start, end) in spans {
        let statement = &source[start..end];
        let definitions = top_level_definitions_in_source(statement);
        let dependencies = identifier_read_facts_in_source(statement)
            .into_iter()
            .map(|fact| BindingName::new(fact.name.as_str()))
            .filter(|binding| all_definitions.contains(binding))
            .collect::<BTreeSet<_>>();
        let prunable = !definitions.is_empty()
            && runtime_orphan_statement_is_prunable(statement, &definitions);

        if definitions.is_empty() || !prunable {
            roots.extend(definitions.iter().cloned());
            roots.extend(dependencies.iter().cloned());
        }

        let index = statements.len();
        for definition in &definitions {
            statements_by_definition
                .entry(definition.clone())
                .or_default()
                .push(index);
        }
        statements.push(RuntimeBindingGraphStatement {
            start,
            end,
            definitions,
            dependencies,
            prunable,
        });
    }

    let mut reachable_bindings = BTreeSet::<BindingName>::new();
    let mut reachable_statements = BTreeSet::<usize>::new();
    let mut stack = roots
        .into_iter()
        .filter(|binding| all_definitions.contains(binding))
        .collect::<Vec<_>>();

    while let Some(binding) = stack.pop() {
        if !reachable_bindings.insert(binding.clone()) {
            continue;
        }
        let Some(statement_indices) = statements_by_definition.get(&binding) else {
            continue;
        };
        for statement_index in statement_indices {
            let statement = &statements[*statement_index];
            if !reachable_statements.insert(*statement_index) {
                continue;
            }
            for definition in statement
                .definitions
                .iter()
                .chain(statement.dependencies.iter())
            {
                if all_definitions.contains(definition) && !reachable_bindings.contains(definition)
                {
                    stack.push(definition.clone());
                }
            }
        }
    }

    let mut dropped_bindings = BTreeSet::<BindingName>::new();
    let edits = statements
        .iter()
        .enumerate()
        .filter_map(|(index, statement)| {
            if !statement.prunable
                || statement.definitions.is_empty()
                || reachable_statements.contains(&index)
            {
                return None;
            }
            dropped_bindings.extend(statement.definitions.iter().cloned());
            Some((statement.start, statement.end, String::new()))
        })
        .collect::<Vec<_>>();

    if edits.is_empty() {
        return RuntimeOrphanPrune {
            source: source.to_string(),
            dropped_bindings,
        };
    }

    RuntimeOrphanPrune {
        source: apply_text_edits(source, &expand_line_removal_edits(source, &edits)),
        dropped_bindings,
    }
}

pub(crate) fn runtime_orphan_statement_is_prunable(
    statement: &str,
    definitions: &BTreeSet<BindingName>,
) -> bool {
    let trimmed = statement.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return false;
    }
    if runtime_orphan_function_declaration_is_prunable(trimmed) {
        return true;
    }
    if keyword_at(trimmed, 0, "class") {
        return pure_class_expression(trimmed);
    }
    runtime_orphan_variable_declaration_is_prunable(trimmed, definitions)
}

pub(crate) fn runtime_orphan_function_declaration_is_prunable(source: &str) -> bool {
    if keyword_at(source, 0, "function") {
        return true;
    }
    if !keyword_at(source, 0, "async") {
        return false;
    }
    let cursor = skip_ws(source.as_bytes(), "async".len());
    keyword_at(source, cursor, "function")
}

pub(crate) fn runtime_orphan_variable_declaration_is_prunable(
    source: &str,
    definitions: &BTreeSet<BindingName>,
) -> bool {
    let Some((_keyword, after_keyword)) = declaration_keyword_at(source, 0) else {
        return false;
    };
    let body = source[skip_ws(source.as_bytes(), after_keyword)..].trim();
    if body.is_empty() {
        return false;
    }
    let mut parsed_bindings = BTreeSet::<BindingName>::new();
    for declarator in split_top_level_properties(body) {
        let declarator = declarator.trim();
        let Some((binding, after_binding)) = parse_identifier(declarator, 0) else {
            return false;
        };
        let binding = BindingName::new(binding);
        if !definitions.contains(&binding) || !parsed_bindings.insert(binding) {
            return false;
        }
        let rest = declarator[skip_ws(declarator.as_bytes(), after_binding)..].trim();
        if rest.is_empty() {
            continue;
        }
        let Some(value) = rest.strip_prefix('=') else {
            return false;
        };
        if value.starts_with('=') || value.starts_with('>') {
            return false;
        }
        if !is_pure_initializer_expression(value.trim()) {
            return false;
        }
    }
    parsed_bindings == *definitions
}
