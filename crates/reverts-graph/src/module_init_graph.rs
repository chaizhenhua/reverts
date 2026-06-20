//! Module initialization-dependency graph — a derived first-class graph.
//!
//! A peer of the module import graph ([`crate::ImportExportGraph`]), the variable
//! def-use graph, and the [`crate::FunctionCallGraph`]. Nodes are emitted modules
//! (by path); a directed edge `A → B` means module `A`'s INIT-TIME code — the
//! statements that run when `A` is evaluated, i.e. its esbuild thunk body and any
//! top-level side-effecting statements — references a binding `A` imports from
//! `B`. Each edge carries a kind: `call` (A calls a function it imports from B at
//! init) and/or `read` (A reads an imported value at init).
//!
//! This REFINES the raw import graph: an import that is only used inside a
//! deferred function (one defined at init but not run until later) is NOT an
//! init-dependency edge, so the init graph's cycles are a subset of the import
//! graph's. The call/read split refines it further — a cycle held together only
//! by `call` edges is resolvable by hoisting + call-graph reachability, while a
//! cycle with `read` edges is a genuine init-order data dependency. Cyclic-init
//! reasoning (e.g. safe de-lazification) should consult THIS graph, filtered by
//! edge kind, rather than the over-approximate import graph.
//!
//! Construction is AST-based (OXC), never textual: imports are read from
//! `ImportDeclaration` specifiers and init-time references from a `Visit` walker
//! that descends into thunk factory bodies and top-level expressions but stops at
//! nested function/arrow boundaries (those are deferred, not init-time).

use std::collections::{BTreeMap, BTreeSet};

use oxc_ast::{
    Visit,
    ast::{
        Argument, ArrowFunctionExpression, CallExpression, Expression, Function,
        IdentifierReference, ImportDeclarationSpecifier, Program, Statement,
    },
};
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_syntax::scope::ScopeFlags;
use reverts_js::parse_options_for;

/// Whether an init-time dependency edge is a function call, a value read, or
/// both. An edge that is purely `call` can often be made eager by hoisting the
/// callee; an edge with `read` is a genuine init-order data dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InitEdgeKinds {
    pub call: bool,
    pub read: bool,
}

impl InitEdgeKinds {
    fn merge(&mut self, other: InitEdgeKinds) {
        self.call |= other.call;
        self.read |= other.read;
    }

    fn matches(self, filter: InitEdgeFilter) -> bool {
        match filter {
            InitEdgeFilter::All => self.call || self.read,
            InitEdgeFilter::ReadOnly => self.read,
            InitEdgeFilter::CallOnly => self.call,
        }
    }
}

/// Which edges to traverse when computing reachability / strongly-connected
/// components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitEdgeFilter {
    /// Every init-time edge (call or read) — the conservative init cycle.
    All,
    /// Only value-read edges — the irreducible init-order data-dependency core.
    ReadOnly,
    /// Only function-call edges.
    CallOnly,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModuleInitGraph {
    paths: Vec<String>,
    index: BTreeMap<String, usize>,
    /// Forward adjacency: `node → (neighbor → edge kinds)`.
    adj: Vec<BTreeMap<usize, InitEdgeKinds>>,
}

impl ModuleInitGraph {
    /// Build the init-dependency graph from the emitted module set. Each item is
    /// `(module_path, source_text)`. Only edges to modules present in the set are
    /// recorded; references to external/package modules are ignored.
    #[must_use]
    pub fn from_emitted_modules<I, P, S>(modules: I) -> Self
    where
        I: IntoIterator<Item = (P, S)>,
        P: Into<String>,
        S: AsRef<str>,
    {
        let modules: Vec<(String, String)> = modules
            .into_iter()
            .map(|(p, s)| (p.into(), s.as_ref().to_string()))
            .collect();
        let mut graph = ModuleInitGraph::default();
        for (path, _) in &modules {
            graph.ensure_node(path);
        }
        for (path, source) in &modules {
            let from = graph.index[path];
            let edges = extract_init_edges(source, path, &graph.index);
            for (to, kinds) in edges {
                graph.record_edge(from, to, kinds);
            }
        }
        graph
    }

    fn ensure_node(&mut self, path: &str) -> usize {
        if let Some(&idx) = self.index.get(path) {
            return idx;
        }
        let idx = self.paths.len();
        self.paths.push(path.to_string());
        self.index.insert(path.to_string(), idx);
        self.adj.push(BTreeMap::new());
        idx
    }

    fn record_edge(&mut self, from: usize, to: usize, kinds: InitEdgeKinds) {
        self.adj[from].entry(to).or_default().merge(kinds);
    }

    /// Test/diagnostic constructor: add a typed edge between two module paths,
    /// creating nodes as needed.
    pub fn insert_edge(&mut self, from: &str, to: &str, kinds: InitEdgeKinds) {
        let from = self.ensure_node(from);
        let to = self.ensure_node(to);
        self.record_edge(from, to, kinds);
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.paths.len()
    }

    #[must_use]
    pub fn path(&self, node: usize) -> Option<&str> {
        self.paths.get(node).map(String::as_str)
    }

    #[must_use]
    pub fn index_of(&self, path: &str) -> Option<usize> {
        self.index.get(path).copied()
    }

    /// Forward init-dependency neighbors of `node` (target → edge kinds).
    #[must_use]
    pub fn dependencies_of(&self, node: usize) -> &BTreeMap<usize, InitEdgeKinds> {
        &self.adj[node]
    }

    /// Total number of directed edges that match `filter`.
    #[must_use]
    pub fn edge_count(&self, filter: InitEdgeFilter) -> usize {
        self.adj
            .iter()
            .flat_map(BTreeMap::values)
            .filter(|kinds| kinds.matches(filter))
            .count()
    }

    /// Strongly-connected components over edges matching `filter`. Each component
    /// is a list of node indices; singletons are included.
    #[must_use]
    pub fn strongly_connected_components(&self, filter: InitEdgeFilter) -> Vec<Vec<usize>> {
        let adjacency: Vec<Vec<usize>> = self
            .adj
            .iter()
            .map(|edges| {
                edges
                    .iter()
                    .filter(|(_, kinds)| kinds.matches(filter))
                    .map(|(&w, _)| w)
                    .collect()
            })
            .collect();
        tarjan_scc(&adjacency)
    }

    /// Node indices that sit in a non-trivial cycle (SCC of size > 1, or a
    /// self-loop) under `filter`.
    #[must_use]
    pub fn cyclic_modules(&self, filter: InitEdgeFilter) -> BTreeSet<usize> {
        let mut cyclic = BTreeSet::new();
        for component in self.strongly_connected_components(filter) {
            let self_loop = component.len() == 1
                && self.adj[component[0]]
                    .get(&component[0])
                    .is_some_and(|kinds| kinds.matches(filter));
            if component.len() > 1 || self_loop {
                cyclic.extend(component);
            }
        }
        cyclic
    }
}

/// Iterative Tarjan SCC over a materialized adjacency list. Returns every SCC
/// (including singletons), in reverse-topological order of the condensation.
fn tarjan_scc(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut sccs = Vec::new();
    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        let mut call_stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(v, child_pos)) = call_stack.last() {
            if child_pos == 0 {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if child_pos < adj[v].len() {
                call_stack.last_mut().unwrap().1 += 1;
                let w = adj[v][child_pos];
                if index[w] == usize::MAX {
                    call_stack.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                if low[v] == index[v] {
                    let mut component = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        component.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(component);
                }
                call_stack.pop();
                if let Some(&(parent, _)) = call_stack.last() {
                    low[parent] = low[parent].min(low[v]);
                }
            }
        }
    }
    sccs
}

/// Resolve a `/`-separated relative specifier against `dir`, mapping `.js` to the
/// emitted `.ts` extension.
fn normalize_module_path(dir: &str, specifier: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in dir.split('/').chain(specifier.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut path = parts.join("/");
    if let Some(stripped) = path.strip_suffix(".js") {
        path = format!("{stripped}.ts");
    } else if !path.ends_with(".ts") {
        path.push_str(".ts");
    }
    path
}

/// Parse `source` and return its init-time edges: imported-module node index →
/// edge kinds. `index` maps a resolved module path to its node index.
fn extract_init_edges(
    source: &str,
    path: &str,
    index: &BTreeMap<String, usize>,
) -> BTreeMap<usize, InitEdgeKinds> {
    let alloc = oxc_allocator::Allocator::default();
    let source_type = SourceType::default().with_typescript(true).with_jsx(true);
    let parsed = Parser::new(&alloc, source, source_type)
        .with_options(parse_options_for(source_type))
        .parse();
    if parsed.panicked {
        return BTreeMap::new();
    }
    let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let imports = import_bindings(&parsed.program, dir, index);
    if imports.is_empty() {
        return BTreeMap::new();
    }
    let mut visitor = InitRefVisitor {
        imports: &imports,
        edges: BTreeMap::new(),
    };
    for statement in &parsed.program.body {
        match statement {
            // Imports/exports and hoisted function declarations are not init-time
            // executable code that can touch a not-yet-initialized binding.
            Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::ExportDefaultDeclaration(_)
            | Statement::FunctionDeclaration(_)
            | Statement::TSTypeAliasDeclaration(_)
            | Statement::TSInterfaceDeclaration(_) => {}
            Statement::VariableDeclaration(declaration) => {
                for declarator in &declaration.declarations {
                    let Some(init) = declarator.init.as_ref() else {
                        continue;
                    };
                    if let Some(body) = thunk_body_statements(init) {
                        for stmt in body {
                            visitor.visit_statement(stmt);
                        }
                    } else {
                        visitor.visit_expression(init);
                    }
                }
            }
            other => visitor.visit_statement(other),
        }
    }
    visitor.edges
}

/// Map every imported local binding name to the node index of the module it is
/// imported from (relative specifiers only, present in `index`).
fn import_bindings(
    program: &Program<'_>,
    dir: &str,
    index: &BTreeMap<String, usize>,
) -> BTreeMap<String, usize> {
    let mut map = BTreeMap::new();
    for statement in &program.body {
        let Statement::ImportDeclaration(import) = statement else {
            continue;
        };
        let specifier = import.source.value.as_str();
        if !(specifier.starts_with("./") || specifier.starts_with("../")) {
            continue;
        }
        let target = normalize_module_path(dir, specifier);
        let Some(&node) = index.get(&target) else {
            continue;
        };
        let Some(specifiers) = import.specifiers.as_ref() else {
            continue;
        };
        for specifier in specifiers {
            let local = match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(s) => s.local.name.as_str(),
                ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => s.local.name.as_str(),
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => s.local.name.as_str(),
            };
            map.insert(local.to_string(), node);
        }
    }
    map
}

/// If `init` is a memoized thunk `MEMO(() => { … })`, return its body statements.
fn thunk_body_statements<'a, 'b>(
    init: &'b Expression<'a>,
) -> Option<&'b oxc_allocator::Vec<'a, Statement<'a>>> {
    let Expression::CallExpression(call) = init else {
        return None;
    };
    if call.arguments.len() != 1 {
        return None;
    }
    let Argument::ArrowFunctionExpression(arrow) = &call.arguments[0] else {
        return None;
    };
    if arrow.expression {
        return None;
    }
    Some(&arrow.body.statements)
}

/// AST walker that records init-time references to imported bindings. It does
/// NOT descend into nested function/arrow bodies — those run later (deferred),
/// not during the enclosing module's initialization.
struct InitRefVisitor<'m> {
    imports: &'m BTreeMap<String, usize>,
    edges: BTreeMap<usize, InitEdgeKinds>,
}

impl InitRefVisitor<'_> {
    fn record(&mut self, name: &str, is_call: bool) {
        if let Some(&node) = self.imports.get(name) {
            let kinds = self.edges.entry(node).or_default();
            if is_call {
                kinds.call = true;
            } else {
                kinds.read = true;
            }
        }
    }
}

impl<'a> Visit<'a> for InitRefVisitor<'_> {
    // Deferred scopes: a function/arrow defined during init runs later, so its
    // body is not an init-time dependency. Stop the walk at these boundaries.
    fn visit_function(&mut self, _func: &Function<'a>, _flags: ScopeFlags) {}
    fn visit_arrow_function_expression(&mut self, _arrow: &ArrowFunctionExpression<'a>) {}

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::Identifier(callee) = &call.callee {
            self.record(callee.name.as_str(), true);
        } else {
            self.visit_expression(&call.callee);
        }
        for argument in &call.arguments {
            self.visit_argument(argument);
        }
    }

    fn visit_identifier_reference(&mut self, ident: &IdentifierReference<'a>) {
        self.record(ident.name.as_str(), false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> InitEdgeKinds {
        InitEdgeKinds {
            call: true,
            read: false,
        }
    }
    fn read() -> InitEdgeKinds {
        InitEdgeKinds {
            call: false,
            read: true,
        }
    }

    #[test]
    fn filtered_scc_separates_call_cycles_from_read_cycles() {
        let mut g = ModuleInitGraph::default();
        // A ⇄ B via CALL edges; C ⇄ D via READ edges.
        g.insert_edge("a.ts", "b.ts", call());
        g.insert_edge("b.ts", "a.ts", call());
        g.insert_edge("c.ts", "d.ts", read());
        g.insert_edge("d.ts", "c.ts", read());

        let all = g.cyclic_modules(InitEdgeFilter::All);
        assert_eq!(all.len(), 4, "all four are cyclic under any-edge");

        // Under READ-only, only the C/D data cycle survives — the call cycle
        // dissolves (resolvable by hoisting).
        let read_cyclic: BTreeSet<&str> = g
            .cyclic_modules(InitEdgeFilter::ReadOnly)
            .into_iter()
            .map(|n| g.path(n).unwrap())
            .collect();
        assert_eq!(read_cyclic, BTreeSet::from(["c.ts", "d.ts"]));

        let call_cyclic: BTreeSet<&str> = g
            .cyclic_modules(InitEdgeFilter::CallOnly)
            .into_iter()
            .map(|n| g.path(n).unwrap())
            .collect();
        assert_eq!(call_cyclic, BTreeSet::from(["a.ts", "b.ts"]));
    }

    #[test]
    fn self_loop_is_cyclic() {
        let mut g = ModuleInitGraph::default();
        g.insert_edge("x.ts", "x.ts", call());
        assert_eq!(g.cyclic_modules(InitEdgeFilter::All).len(), 1);
        assert!(g.cyclic_modules(InitEdgeFilter::ReadOnly).is_empty());
    }

    #[test]
    fn merged_edge_carries_both_kinds() {
        let mut g = ModuleInitGraph::default();
        g.insert_edge("a.ts", "b.ts", call());
        g.insert_edge("a.ts", "b.ts", read());
        let a = g.index_of("a.ts").unwrap();
        let b = g.index_of("b.ts").unwrap();
        let kinds = g.dependencies_of(a)[&b];
        assert!(kinds.call && kinds.read);
        assert_eq!(g.edge_count(InitEdgeFilter::All), 1);
        assert_eq!(g.edge_count(InitEdgeFilter::ReadOnly), 1);
    }

    #[test]
    fn builder_reads_call_vs_read_edges_from_ast() {
        // entry imports `f` (called at init) and `v` (read at init) from dep;
        // and `g` from late, used only inside a deferred function (NOT init).
        let entry = concat!(
            "import { f, v } from './dep.js';\n",
            "import { g } from './late.js';\n",
            "var thunk = _$l(() => { f(); var x = v; });\n",
            "function deferred() { return g(); }\n",
            "export { thunk };\n",
        );
        let dep = "export var f = () => 1;\nexport var v = 2;\n";
        let late = "export var g = () => 3;\n";
        let graph = ModuleInitGraph::from_emitted_modules([
            ("entry.ts", entry),
            ("dep.ts", dep),
            ("late.ts", late),
        ]);
        let e = graph.index_of("entry.ts").unwrap();
        let d = graph.index_of("dep.ts").unwrap();
        let deps = graph.dependencies_of(e);
        // edge entry → dep carries BOTH call (f) and read (v)
        assert!(deps[&d].call, "f() is an init-time call edge");
        assert!(deps[&d].read, "v is an init-time read edge");
        // entry → late must NOT exist: `g` is referenced only in a deferred
        // function body, not at init time.
        let late = graph.index_of("late.ts").unwrap();
        assert!(
            !deps.contains_key(&late),
            "deferred-only references are not init dependencies"
        );
    }

    #[test]
    fn builder_finds_init_cycle_only_through_init_time_use() {
        // a ⇄ b mutually import. a calls b's init function at init time; b only
        // uses a's import inside a deferred function. So the IMPORT graph has a
        // cycle but the INIT graph does not (b→a is not an init edge).
        let a = concat!(
            "import { binit } from './b.js';\n",
            "var t = _$l(() => { binit(); });\n",
            "export var ainit = () => 1;\n",
        );
        let b = concat!(
            "import { ainit } from './a.js';\n",
            "export var binit = () => 1;\n",
            "function later() { return ainit(); }\n",
        );
        let graph = ModuleInitGraph::from_emitted_modules([("a.ts", a), ("b.ts", b)]);
        // a → b edge exists (call), b → a does NOT (deferred use only).
        let ai = graph.index_of("a.ts").unwrap();
        let bi = graph.index_of("b.ts").unwrap();
        assert!(graph.dependencies_of(ai).contains_key(&bi));
        assert!(!graph.dependencies_of(bi).contains_key(&ai));
        // ...so there is no init cycle, even though the import graph is cyclic.
        assert!(graph.cyclic_modules(InitEdgeFilter::All).is_empty());
    }
}
