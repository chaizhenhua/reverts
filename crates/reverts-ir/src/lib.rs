use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

mod byte_range;
pub use byte_range::{ByteRange, FunctionId};

mod fingerprint;
pub use fingerprint::{
    AlternateAxisHashes, AxisHashes, AxisKind, FunctionFingerprint, NormalizationPassId,
};

pub mod hash;

#[must_use]
pub fn is_ascii_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

#[must_use]
pub fn is_ascii_identifier_continue(byte: u8) -> bool {
    is_ascii_identifier_start(byte) || byte.is_ascii_digit()
}

#[must_use]
pub fn is_identifier_like_ascii(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && is_ascii_identifier_start(bytes[0])
        && bytes[1..]
            .iter()
            .all(|byte| is_ascii_identifier_continue(*byte))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BindingName(String);

impl BindingName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BindingName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    Application,
    Package,
    Builtin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRecord {
    pub id: ModuleId,
    pub kind: ModuleKind,
    pub semantic_path: String,
    pub package_name: Option<String>,
    pub package_version: Option<String>,
}

impl ModuleRecord {
    #[must_use]
    pub fn application(id: ModuleId, semantic_path: impl Into<String>) -> Self {
        Self {
            id,
            kind: ModuleKind::Application,
            semantic_path: semantic_path.into(),
            package_name: None,
            package_version: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BindingShape {
    Unknown,
    Value,
    PlainObject,
    NamespaceObject,
    EnumObject,
    Callable,
    Constructor,
    ClassLike,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InferredType {
    Unknown,
    Never,
    String,
    Number,
    Boolean,
    BigInt,
    Null,
    Undefined,
}

impl InferredType {
    #[must_use]
    pub const fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown)
    }

    #[must_use]
    pub fn join(self, other: Self) -> Self {
        if self == other {
            self
        } else if self == Self::Never {
            other
        } else if other == Self::Never {
            self
        } else {
            Self::Unknown
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TypeSolution {
    types: BTreeMap<(ModuleId, BindingName), InferredType>,
}

impl TypeSolution {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, module_id: ModuleId, binding: impl Into<String>, ty: InferredType) {
        let key = (module_id, BindingName::new(binding));
        self.types
            .entry(key)
            .and_modify(|existing| *existing = existing.join(ty))
            .or_insert(ty);
    }

    #[must_use]
    pub fn type_of(&self, module_id: ModuleId, binding: &str) -> InferredType {
        self.types
            .get(&(module_id, BindingName::new(binding)))
            .copied()
            .unwrap_or(InferredType::Unknown)
    }

    #[must_use]
    pub fn types_for_module(&self, module_id: ModuleId) -> Vec<(BindingName, InferredType)> {
        self.types
            .iter()
            .filter_map(|((typed_module, binding), ty)| {
                (*typed_module == module_id).then_some((binding.clone(), *ty))
            })
            .collect()
    }
}

impl BindingShape {
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        self.max(other)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BindingConstraintKind {
    Read,
    Call,
    Construct,
    MemberRead,
    MemberWrite,
    ObjectLiteralDeclaration,
    EnumInitializer,
    ClassDeclaration,
}

impl BindingConstraintKind {
    #[must_use]
    pub fn required_shape(self) -> BindingShape {
        match self {
            Self::Read => BindingShape::Value,
            Self::Call => BindingShape::Callable,
            Self::Construct => BindingShape::Constructor,
            Self::MemberRead | Self::MemberWrite => BindingShape::NamespaceObject,
            Self::ObjectLiteralDeclaration => BindingShape::PlainObject,
            Self::EnumInitializer => BindingShape::EnumObject,
            Self::ClassDeclaration => BindingShape::ClassLike,
        }
    }

    #[must_use]
    pub fn conflicts_with(self, other: Self) -> bool {
        use BindingConstraintKind::{
            Call, ClassDeclaration, Construct, EnumInitializer, MemberRead, MemberWrite,
            ObjectLiteralDeclaration, Read,
        };

        let self_kind = match self {
            Read | MemberRead | MemberWrite => return false,
            ObjectLiteralDeclaration => ObjectLiteralDeclaration,
            EnumInitializer => EnumInitializer,
            Call => Call,
            Construct => Construct,
            ClassDeclaration => ClassDeclaration,
        };
        let other_kind = match other {
            Read | MemberRead | MemberWrite => return false,
            ObjectLiteralDeclaration => ObjectLiteralDeclaration,
            EnumInitializer => EnumInitializer,
            Call => Call,
            Construct => Construct,
            ClassDeclaration => ClassDeclaration,
        };

        matches!(
            (self_kind, other_kind),
            (
                ObjectLiteralDeclaration,
                Call | Construct | ClassDeclaration | EnumInitializer
            ) | (
                Call | Construct | ClassDeclaration | EnumInitializer,
                ObjectLiteralDeclaration
            ) | (EnumInitializer, Call | Construct | ClassDeclaration)
                | (Call | Construct | ClassDeclaration, EnumInitializer)
                | (ClassDeclaration, Call)
                | (Call, ClassDeclaration)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingConstraint {
    pub module_id: ModuleId,
    pub binding: BindingName,
    pub kind: BindingConstraintKind,
    /// Optional property name when the constraint records a member access
    /// on `binding` (e.g. `ns.foo` records `property = Some("foo")`).
    /// Paper #7 (Anderson) — required to materialise NamespaceObject
    /// shapes with known members. Default `None` keeps existing callers
    /// constructing shape-only constraints unchanged.
    pub property: Option<BindingName>,
}

impl BindingConstraint {
    #[must_use]
    pub fn new(
        module_id: ModuleId,
        binding: impl Into<String>,
        kind: BindingConstraintKind,
    ) -> Self {
        Self {
            module_id,
            binding: BindingName::new(binding),
            kind,
            property: None,
        }
    }

    /// Attach a property name to a member-access constraint.
    #[must_use]
    pub fn with_property(mut self, property: impl Into<String>) -> Self {
        self.property = Some(BindingName::new(property));
        self
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DefUseGraph {
    definitions: BTreeSet<(ModuleId, BindingName)>,
    imports: BTreeSet<(ModuleId, BindingName)>,
    reads: BTreeSet<(ModuleId, BindingName)>,
    writes: BTreeSet<(ModuleId, BindingName)>,
    constraints: Vec<BindingConstraint>,
    /// Bindings observed to be written from a member-access chain on an
    /// awaited or called value. The chain is statically nullable — e.g.
    /// `X = (await fetch(...)).data.value` could leave `X` as `null` or
    /// `undefined`. Used by the `UnprotectedNullableMemberRead` audit.
    maybe_nullable_writes: BTreeSet<(ModuleId, BindingName)>,
    /// `target = source` identity edges. `target` inherits `source`'s
    /// shape / known members / nullable status — without inlining, this
    /// is the only way for the dataflow propagation to follow renames.
    identity_aliases: BTreeSet<(ModuleId, BindingName, BindingName)>,
    /// `target = callee(...)` edges. Combined with `function_returns`,
    /// these turn `let A = F();` into the alias `A ← (anything F returns)`
    /// so that information flowing into F's return survives the call.
    call_aliases: BTreeSet<(ModuleId, BindingName, BindingName)>,
    /// For a function binding `F`, the set of bindings that `F` is
    /// observed to `return` directly. Populated by the AST visitor on
    /// `return X;` statements at function-decl scope.
    function_returns: BTreeMap<(ModuleId, BindingName), BTreeSet<BindingName>>,
}

impl DefUseGraph {
    pub fn define(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.definitions
            .insert((module_id, BindingName::new(binding)));
    }

    pub fn import(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.imports.insert((module_id, BindingName::new(binding)));
    }

    pub fn read(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.reads.insert((module_id, BindingName::new(binding)));
    }

    pub fn write(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.writes.insert((module_id, BindingName::new(binding)));
    }

    pub fn constrain(&mut self, constraint: BindingConstraint) {
        self.reads
            .insert((constraint.module_id, constraint.binding.clone()));
        self.constraints.push(constraint);
    }

    /// Record that `(module_id, binding)` was assigned from a member-access
    /// chain on an awaited / called value (statically nullable RHS). Used
    /// by the `UnprotectedNullableMemberRead` audit; no shape effect.
    pub fn record_maybe_nullable_write(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.maybe_nullable_writes
            .insert((module_id, BindingName::new(binding)));
    }

    #[must_use]
    pub fn maybe_nullable_writes(&self) -> &BTreeSet<(ModuleId, BindingName)> {
        &self.maybe_nullable_writes
    }

    /// Record `target = source` — `target` inherits any propagatable info
    /// from `source` via the alias closure.
    pub fn record_identity_alias(
        &mut self,
        module_id: ModuleId,
        target: impl Into<String>,
        source: impl Into<String>,
    ) {
        self.identity_aliases.insert((
            module_id,
            BindingName::new(target),
            BindingName::new(source),
        ));
    }

    /// Record `target = callee(...)` — `target` inherits info from whatever
    /// `callee` returns. Resolved against `function_returns` at query time.
    pub fn record_call_alias(
        &mut self,
        module_id: ModuleId,
        target: impl Into<String>,
        callee: impl Into<String>,
    ) {
        self.call_aliases.insert((
            module_id,
            BindingName::new(target),
            BindingName::new(callee),
        ));
    }

    /// Record `function F() { return X; }` — the call-alias closure uses
    /// this map to thread `X` through any `Y = F()` callers.
    pub fn record_function_return(
        &mut self,
        module_id: ModuleId,
        function: impl Into<String>,
        returned: impl Into<String>,
    ) {
        self.function_returns
            .entry((module_id, BindingName::new(function)))
            .or_default()
            .insert(BindingName::new(returned));
    }

    /// Bindings whose information flows into `(module_id, binding)` via
    /// identity assignments and call/return composition. Includes
    /// `binding` itself. Stops at fixed-point (lattice = subset on a
    /// finite set, so termination is guaranteed).
    #[must_use]
    pub fn alias_sources_of(&self, module_id: ModuleId, binding: &str) -> BTreeSet<BindingName> {
        let start = BindingName::new(binding);
        let mut seen = BTreeSet::new();
        seen.insert(start.clone());
        let mut work = vec![start];
        while let Some(current) = work.pop() {
            for (m, target, source) in &self.identity_aliases {
                if *m == module_id && *target == current && seen.insert(source.clone()) {
                    work.push(source.clone());
                }
            }
            for (m, target, callee) in &self.call_aliases {
                if *m != module_id || *target != current {
                    continue;
                }
                let key = (module_id, callee.clone());
                if let Some(returned) = self.function_returns.get(&key) {
                    for returned_binding in returned {
                        if seen.insert(returned_binding.clone()) {
                            work.push(returned_binding.clone());
                        }
                    }
                }
            }
        }
        seen
    }

    #[must_use]
    pub fn has_definition_or_import(&self, module_id: ModuleId, binding: &BindingName) -> bool {
        self.definitions.contains(&(module_id, binding.clone()))
            || self.imports.contains(&(module_id, binding.clone()))
    }

    /// Modules that contain a top-level *definition* of `binding` (imports are
    /// excluded — an importer is not the owner). Used to attribute a free read
    /// in one module back to the module(s) that actually own the symbol.
    #[must_use]
    pub fn modules_defining(&self, binding: &BindingName) -> BTreeSet<ModuleId> {
        self.definitions
            .iter()
            .filter_map(|(module_id, defined)| (defined == binding).then_some(*module_id))
            .collect()
    }

    #[must_use]
    pub fn unresolved_reads(&self) -> Vec<(ModuleId, BindingName)> {
        self.reads
            .iter()
            .filter(|(module_id, binding)| !self.has_definition_or_import(*module_id, binding))
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn unresolved_writes(&self) -> Vec<(ModuleId, BindingName)> {
        self.writes
            .iter()
            .filter(|(module_id, binding)| !self.has_definition_or_import(*module_id, binding))
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn constraints(&self) -> &[BindingConstraint] {
        &self.constraints
    }

    /// Collect the deduplicated set of property names accessed on
    /// `(module_id, binding)` across all member-access constraints. Returns
    /// an empty set when the binding has no recorded property accesses.
    #[must_use]
    pub fn members_accessed_on(&self, module_id: ModuleId, binding: &str) -> BTreeSet<BindingName> {
        let target = BindingName::new(binding);
        self.constraints
            .iter()
            .filter_map(|constraint| {
                if constraint.module_id != module_id || constraint.binding != target {
                    return None;
                }
                constraint.property.clone()
            })
            .collect()
    }

    #[must_use]
    pub fn data_dependence_edges(&self) -> Vec<DataDependenceEdge> {
        let mut edges = Vec::new();
        for (module_id, binding) in &self.reads {
            self.push_data_dependence_edge(&mut edges, *module_id, binding, BindingUseKind::Read);
        }
        for (module_id, binding) in &self.writes {
            self.push_data_dependence_edge(&mut edges, *module_id, binding, BindingUseKind::Write);
        }
        edges
    }

    fn push_data_dependence_edge(
        &self,
        edges: &mut Vec<DataDependenceEdge>,
        module_id: ModuleId,
        binding: &BindingName,
        target: BindingUseKind,
    ) {
        let source = if self.definitions.contains(&(module_id, binding.clone())) {
            BindingSourceKind::Definition
        } else if self.imports.contains(&(module_id, binding.clone())) {
            BindingSourceKind::Import
        } else {
            return;
        };

        edges.push(DataDependenceEdge {
            module_id,
            binding: binding.clone(),
            source,
            target,
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BindingSourceKind {
    Definition,
    Import,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BindingUseKind {
    Read,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDependenceEdge {
    pub module_id: ModuleId,
    pub binding: BindingName,
    pub source: BindingSourceKind,
    pub target: BindingUseKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FlowNodeId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ControlFlowNodeKind {
    Entry,
    Statement,
    Branch,
    Loop,
    Return,
    Throw,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ControlFlowEdgeKind {
    Entry,
    Sequential,
    Conditional,
    LoopBack,
    Termination,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlFlowNode {
    pub id: FlowNodeId,
    pub module_id: ModuleId,
    pub kind: ControlFlowNodeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlFlowEdge {
    pub module_id: ModuleId,
    pub from: FlowNodeId,
    pub to: FlowNodeId,
    pub kind: ControlFlowEdgeKind,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ControlFlowGraph {
    next_node_id: u32,
    nodes: BTreeMap<ModuleId, Vec<ControlFlowNode>>,
    edges: BTreeMap<ModuleId, Vec<ControlFlowEdge>>,
}

impl ControlFlowGraph {
    pub fn add_node(&mut self, module_id: ModuleId, kind: ControlFlowNodeKind) -> FlowNodeId {
        let id = FlowNodeId(self.next_node_id);
        self.next_node_id += 1;
        self.nodes
            .entry(module_id)
            .or_default()
            .push(ControlFlowNode {
                id,
                module_id,
                kind,
            });
        id
    }

    pub fn add_edge(
        &mut self,
        module_id: ModuleId,
        from: FlowNodeId,
        to: FlowNodeId,
        kind: ControlFlowEdgeKind,
    ) {
        self.edges
            .entry(module_id)
            .or_default()
            .push(ControlFlowEdge {
                module_id,
                from,
                to,
                kind,
            });
    }

    pub fn extend(&mut self, other: Self) {
        for (module_id, nodes) in other.nodes {
            self.nodes.entry(module_id).or_default().extend(nodes);
        }
        for (module_id, edges) in other.edges {
            self.edges.entry(module_id).or_default().extend(edges);
        }
        self.next_node_id = self.next_node_id.max(other.next_node_id);
    }

    #[must_use]
    pub fn nodes_for(&self, module_id: ModuleId) -> &[ControlFlowNode] {
        self.nodes
            .get(&module_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    #[must_use]
    pub fn edges_for(&self, module_id: ModuleId) -> &[ControlFlowEdge] {
        self.edges
            .get(&module_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

/// Module-local identifier for a scope-resolved binding, projected from
/// oxc's `SymbolTable` (the numeric symbol index within one module's
/// semantic build). Stable and deterministic for a given source, but only
/// meaningful within its owning `ModuleId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResolvedSymbolId(pub u32);

/// How a binding entered scope, projected from oxc's `SymbolFlags`. Captures
/// the declaration form the decompiler needs to faithfully re-emit a symbol —
/// in particular `const`/`let`/`var`, kept distinct from one another and from
/// functions, classes, imports, and type-only declarations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeclarationKind {
    Const,
    Let,
    Var,
    Parameter,
    Function,
    Class,
    Import,
    CatchParam,
    Enum,
    Interface,
    TypeAlias,
    TypeParameter,
    Other,
}

impl DeclarationKind {
    /// `true` for `const`/`let`/`var` value declarations.
    #[must_use]
    pub fn is_variable(self) -> bool {
        matches!(self, Self::Const | Self::Let | Self::Var)
    }

    /// `true` for forms that re-emit as a `const` binding when never reassigned
    /// (`let`/`var`). `const` is already const; everything else keeps its form.
    #[must_use]
    pub fn is_const_upgradeable(self) -> bool {
        matches!(self, Self::Let | Self::Var)
    }
}

/// A scope-resolved binding occurrence.
///
/// Unlike [`DefUseGraph`], which keys everything by `(ModuleId, BindingName)`
/// and therefore collapses every same-named binding in a module to one node,
/// each `ResolvedBinding` carries a distinct [`ResolvedSymbolId`] and its
/// lexical depth — so the minified locals (`t`, `e`, `n`) that esbuild reuses
/// across functions never conflate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBinding {
    pub symbol: ResolvedSymbolId,
    pub name: BindingName,
    /// Lexical nesting of the binding's declaring scope. `0` is the module
    /// (program root) scope; deeper bindings live inside functions/blocks.
    pub scope_depth: u32,
    pub declaration: DeclarationKind,
    /// Whether the binding is ever written after its declaration (oxc's own
    /// mutation tracking). A `const` is never reassigned; a `let`/`var` that
    /// is never reassigned can be re-emitted as `const`.
    pub reassigned: bool,
    /// Number of scope-resolved references that read this exact binding. Unlike
    /// a name-keyed read count, two same-named bindings in different scopes are
    /// counted independently.
    pub read_count: u32,
    /// Number of scope-resolved references that write this exact binding.
    pub write_count: u32,
}

impl ResolvedBinding {
    #[must_use]
    pub fn is_module_scope(&self) -> bool {
        self.scope_depth == 0
    }

    /// `true` when this binding was declared `let`/`var` but is never
    /// reassigned, so it can be faithfully re-emitted as `const`.
    #[must_use]
    pub fn can_be_const(&self) -> bool {
        self.declaration.is_const_upgradeable() && !self.reassigned
    }

    /// `true` when no resolved reference ever reads this binding. A scope-correct
    /// dead-binding signal: the name-keyed graph cannot distinguish an unread
    /// local from a same-named binding that is read elsewhere.
    #[must_use]
    pub fn is_unread(&self) -> bool {
        self.read_count == 0
    }
}

/// Scope-resolved projection of a program's bindings, keyed by module and
/// [`ResolvedSymbolId`]. A first-class peer of [`DefUseGraph`] that adds the
/// scope identity the name-keyed graph cannot represent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResolvedSymbolGraph {
    bindings: BTreeMap<ModuleId, BTreeMap<ResolvedSymbolId, ResolvedBinding>>,
    /// `(module, name) → symbols` reverse index. More than one entry means the
    /// name is reused across scopes within that module.
    by_name: BTreeMap<(ModuleId, BindingName), BTreeSet<ResolvedSymbolId>>,
}

impl ResolvedSymbolGraph {
    pub fn record(&mut self, module_id: ModuleId, binding: ResolvedBinding) {
        self.by_name
            .entry((module_id, binding.name.clone()))
            .or_default()
            .insert(binding.symbol);
        self.bindings
            .entry(module_id)
            .or_default()
            .insert(binding.symbol, binding);
    }

    /// Merge another graph into this one (modules are disjoint in practice;
    /// later inserts win on collision).
    pub fn extend(&mut self, other: Self) {
        for (module_id, bindings) in other.bindings {
            for binding in bindings.into_values() {
                self.record(module_id, binding);
            }
        }
    }

    #[must_use]
    pub fn binding(
        &self,
        module_id: ModuleId,
        symbol: ResolvedSymbolId,
    ) -> Option<&ResolvedBinding> {
        self.bindings.get(&module_id)?.get(&symbol)
    }

    /// All bindings in `module_id` that share `name`, ordered by symbol id.
    /// Length > 1 proves the name is reused across distinct scopes.
    #[must_use]
    pub fn symbols_named(&self, module_id: ModuleId, name: &str) -> Vec<&ResolvedBinding> {
        let key = (module_id, BindingName::new(name));
        let Some(symbols) = self.by_name.get(&key) else {
            return Vec::new();
        };
        let module_bindings = self.bindings.get(&module_id);
        symbols
            .iter()
            .filter_map(|symbol| module_bindings.and_then(|m| m.get(symbol)))
            .collect()
    }

    /// `true` when `name` resolves to more than one distinct binding in
    /// `module_id` — a name-keyed lookup would conflate them.
    #[must_use]
    pub fn name_is_ambiguous(&self, module_id: ModuleId, name: &str) -> bool {
        self.by_name
            .get(&(module_id, BindingName::new(name)))
            .is_some_and(|symbols| symbols.len() > 1)
    }

    /// The unique module-scope binding named `name`, if exactly one exists at
    /// depth 0. Returns `None` when the name is absent at module scope or is
    /// declared more than once there (e.g. a `var`/`function` merge).
    #[must_use]
    pub fn module_scope_binding(
        &self,
        module_id: ModuleId,
        name: &str,
    ) -> Option<&ResolvedBinding> {
        let mut module_scope = self
            .symbols_named(module_id, name)
            .into_iter()
            .filter(|binding| binding.is_module_scope());
        let first = module_scope.next()?;
        module_scope.next().is_none().then_some(first)
    }

    /// Module-scope (top-level) bindings of `module_id`, ordered by symbol id.
    #[must_use]
    pub fn module_scope_bindings(&self, module_id: ModuleId) -> Vec<&ResolvedBinding> {
        self.bindings
            .get(&module_id)
            .map(|bindings| {
                bindings
                    .values()
                    .filter(|binding| binding.is_module_scope())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Bindings in `module_id` that no resolved reference ever reads, ordered by
    /// symbol id. Scope-correct dead-binding candidates.
    #[must_use]
    pub fn unread_bindings(&self, module_id: ModuleId) -> Vec<&ResolvedBinding> {
        self.bindings
            .get(&module_id)
            .map(|bindings| {
                bindings
                    .values()
                    .filter(|binding| binding.is_unread())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn bindings_for(&self, module_id: ModuleId) -> Vec<&ResolvedBinding> {
        self.bindings
            .get(&module_id)
            .map(|bindings| bindings.values().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.values().all(BTreeMap::is_empty)
    }

    #[must_use]
    pub fn binding_count(&self) -> usize {
        self.bindings.values().map(BTreeMap::len).sum()
    }
}

/// Facts derived from the intraprocedural control-flow graph (oxc `with_cfg`),
/// projected into owned, deterministic IR. A peer of [`ResolvedSymbolGraph`]
/// that adds the flow-sensitive dimension the def-use / resolved graphs lack:
/// *where*, on which paths, a fact holds — not just *whether* it ever holds.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IntraproceduralFlow {
    /// Source spans of statements that are unreachable within their enclosing
    /// function (dead code after `return`/`throw`/`break`/`continue`, in any
    /// branch), ordered by position. Computed from oxc CFG reachability — it
    /// sees inside function bodies and nested blocks, unlike the top-level
    /// [`ControlFlowGraph`] statement skeleton.
    unreachable: BTreeMap<ModuleId, Vec<ByteRange>>,
}

impl IntraproceduralFlow {
    /// Record an unreachable-code span. Spans are kept sorted and deduplicated
    /// per module so the projection is deterministic regardless of CFG
    /// traversal order.
    pub fn add_unreachable(&mut self, module_id: ModuleId, span: ByteRange) {
        let spans = self.unreachable.entry(module_id).or_default();
        if let Err(index) = spans.binary_search(&span) {
            spans.insert(index, span);
        }
    }

    pub fn extend(&mut self, other: Self) {
        for (module_id, spans) in other.unreachable {
            for span in spans {
                self.add_unreachable(module_id, span);
            }
        }
    }

    /// Unreachable-code spans in `module_id`, ordered by position.
    #[must_use]
    pub fn unreachable_in(&self, module_id: ModuleId) -> &[ByteRange] {
        self.unreachable
            .get(&module_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    #[must_use]
    pub fn has_unreachable(&self, module_id: ModuleId) -> bool {
        self.unreachable
            .get(&module_id)
            .is_some_and(|spans| !spans.is_empty())
    }

    #[must_use]
    pub fn unreachable_count(&self) -> usize {
        self.unreachable.values().map(Vec::len).sum()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BindingShapeSolution {
    shapes: BTreeMap<(ModuleId, BindingName), BindingShape>,
    constraint_kinds: BTreeMap<(ModuleId, BindingName), BTreeSet<BindingConstraintKind>>,
    conflicts: Vec<BindingShapeConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingShapeConflict {
    pub module_id: ModuleId,
    pub binding: BindingName,
    pub existing_kind: BindingConstraintKind,
    pub incoming_kind: BindingConstraintKind,
    pub existing_shape: BindingShape,
    pub incoming_shape: BindingShape,
}

impl BindingShapeSolution {
    #[must_use]
    pub fn from_def_use_graph(graph: &DefUseGraph) -> Self {
        let mut solution = Self::default();
        for constraint in graph.constraints() {
            solution.add_constraint(constraint);
        }
        solution.propagate_shapes_through_aliases(graph);
        solution
    }

    /// Walk the alias graph (identity assignments + call/return composition)
    /// and merge shapes bidirectionally across each edge. `A = X` aliases
    /// the same runtime object — both bindings should observe the same
    /// shape lattice point. Fixed-point because `BindingShape::merge =
    /// max` is monotone over a finite lattice.
    fn propagate_shapes_through_aliases(&mut self, graph: &DefUseGraph) {
        loop {
            let mut changed = false;
            for (module_id, target, source) in &graph.identity_aliases {
                if self.merge_pair(*module_id, target, source) {
                    changed = true;
                }
            }
            for (module_id, target, callee) in &graph.call_aliases {
                let key = (*module_id, callee.clone());
                let Some(returned_set) = graph.function_returns.get(&key) else {
                    continue;
                };
                for returned in returned_set {
                    if self.merge_pair(*module_id, target, returned) {
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Merge shapes between two aliased bindings (bidirectional).
    /// Returns `true` if either binding's shape changed.
    fn merge_pair(&mut self, module_id: ModuleId, left: &BindingName, right: &BindingName) -> bool {
        let left_key = (module_id, left.clone());
        let right_key = (module_id, right.clone());
        let left_shape = self
            .shapes
            .get(&left_key)
            .copied()
            .unwrap_or(BindingShape::Unknown);
        let right_shape = self
            .shapes
            .get(&right_key)
            .copied()
            .unwrap_or(BindingShape::Unknown);
        let merged = left_shape.merge(right_shape);
        let mut changed = false;
        if merged != left_shape {
            self.shapes.insert(left_key, merged);
            changed = true;
        }
        if merged != right_shape {
            self.shapes.insert(right_key, merged);
            changed = true;
        }
        changed
    }

    pub fn add_constraint(&mut self, constraint: &BindingConstraint) {
        let key = (constraint.module_id, constraint.binding.clone());
        let required = constraint.kind.required_shape();
        let existing_kinds = self.constraint_kinds.entry(key.clone()).or_default();
        if existing_kinds.insert(constraint.kind) {
            self.conflicts
                .extend(existing_kinds.iter().copied().filter_map(|existing_kind| {
                    if existing_kind == constraint.kind
                        || !existing_kind.conflicts_with(constraint.kind)
                    {
                        return None;
                    }

                    Some(BindingShapeConflict {
                        module_id: constraint.module_id,
                        binding: constraint.binding.clone(),
                        existing_kind,
                        incoming_kind: constraint.kind,
                        existing_shape: existing_kind.required_shape(),
                        incoming_shape: required,
                    })
                }));
        }
        self.shapes
            .entry(key)
            .and_modify(|shape| *shape = shape.merge(required))
            .or_insert(required);
    }

    #[must_use]
    pub fn shape_of(&self, module_id: ModuleId, binding: &str) -> BindingShape {
        self.shapes
            .get(&(module_id, BindingName::new(binding)))
            .copied()
            .unwrap_or(BindingShape::Unknown)
    }

    #[must_use]
    pub fn conflicts(&self) -> &[BindingShapeConflict] {
        &self.conflicts
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSurface {
    pub package_name: String,
    root_importable: bool,
    subpaths: BTreeSet<String>,
}

impl PackageSurface {
    #[must_use]
    pub fn new(package_name: impl Into<String>) -> Self {
        Self {
            package_name: package_name.into(),
            root_importable: false,
            subpaths: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn with_root_importable(mut self) -> Self {
        self.root_importable = true;
        self
    }

    #[must_use]
    pub fn with_subpath(mut self, subpath: impl Into<String>) -> Self {
        self.subpaths.insert(normalize_subpath(&subpath.into()));
        self
    }

    #[must_use]
    pub fn accepts(&self, specifier: &str) -> bool {
        let Some((package_name, subpath)) = split_bare_specifier(specifier) else {
            return false;
        };
        if package_name != self.package_name {
            return false;
        }
        match subpath {
            None => self.root_importable,
            Some(subpath) => self.subpaths.contains(&normalize_subpath(&subpath)),
        }
    }
}

#[must_use]
pub fn split_bare_specifier(specifier: &str) -> Option<(String, Option<String>)> {
    if specifier.starts_with('.') || specifier.starts_with('/') || specifier.is_empty() {
        return None;
    }

    let parts = specifier.split('/').collect::<Vec<_>>();
    if specifier.starts_with('@') {
        if parts.len() < 2 || parts[0].len() <= 1 || parts[1].is_empty() {
            return None;
        }
        let package = format!("{}/{}", parts[0], parts[1]);
        let subpath = (parts.len() > 2).then(|| parts[2..].join("/"));
        Some((package, subpath))
    } else {
        let package = parts[0].to_string();
        let subpath = (parts.len() > 1).then(|| parts[1..].join("/"));
        Some((package, subpath))
    }
}

#[must_use]
pub fn is_valid_package_name(value: &str) -> bool {
    let Some((package_name, subpath)) = split_bare_specifier(value) else {
        return false;
    };
    subpath.is_none()
        && package_name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'@' | b'/' | b'.' | b'_' | b'-')
        })
}

fn normalize_subpath(subpath: &str) -> String {
    subpath.trim_start_matches("./").to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        BindingConstraint, BindingConstraintKind, BindingName, BindingShape, BindingShapeSolution,
        BindingSourceKind, BindingUseKind, ByteRange, ControlFlowEdgeKind, ControlFlowGraph,
        ControlFlowNodeKind, DeclarationKind, DefUseGraph, IntraproceduralFlow, ModuleId,
        PackageSurface, ResolvedBinding, ResolvedSymbolGraph, ResolvedSymbolId,
        is_valid_package_name,
    };

    fn resolved(symbol: u32, name: &str, scope_depth: u32) -> ResolvedBinding {
        ResolvedBinding {
            symbol: ResolvedSymbolId(symbol),
            name: BindingName::new(name),
            scope_depth,
            declaration: DeclarationKind::Let,
            reassigned: false,
            read_count: 0,
            write_count: 0,
        }
    }

    #[test]
    fn resolved_symbol_graph_disambiguates_same_name_across_scopes() {
        let mut graph = ResolvedSymbolGraph::default();
        graph.record(ModuleId(1), resolved(0, "t", 0));
        graph.record(ModuleId(1), resolved(1, "t", 1));
        graph.record(ModuleId(1), resolved(2, "t", 2));
        graph.record(ModuleId(1), resolved(3, "unique", 0));
        // A different module reusing the same symbol ids must not leak in.
        graph.record(ModuleId(2), resolved(0, "t", 0));

        let module_one = graph.symbols_named(ModuleId(1), "t");
        assert_eq!(module_one.len(), 3);
        assert!(graph.name_is_ambiguous(ModuleId(1), "t"));
        assert!(!graph.name_is_ambiguous(ModuleId(1), "unique"));
        assert!(!graph.name_is_ambiguous(ModuleId(2), "t"));

        let module_scope = graph.module_scope_bindings(ModuleId(1));
        assert_eq!(module_scope.len(), 2, "t@0 and unique@0");
        assert!(module_scope.iter().all(|binding| binding.is_module_scope()));

        assert_eq!(
            graph
                .binding(ModuleId(1), ResolvedSymbolId(2))
                .map(|binding| binding.scope_depth),
            Some(2),
        );
        assert_eq!(graph.binding_count(), 5);
    }

    #[test]
    fn resolved_binding_const_upgrade_and_module_scope_lookup() {
        let mut graph = ResolvedSymbolGraph::default();
        graph.record(
            ModuleId(1),
            ResolvedBinding {
                symbol: ResolvedSymbolId(0),
                name: BindingName::new("immutable"),
                scope_depth: 0,
                declaration: DeclarationKind::Let,
                reassigned: false,
                read_count: 1,
                write_count: 0,
            },
        );
        graph.record(
            ModuleId(1),
            ResolvedBinding {
                symbol: ResolvedSymbolId(1),
                name: BindingName::new("counter"),
                scope_depth: 0,
                declaration: DeclarationKind::Var,
                reassigned: true,
                read_count: 0,
                write_count: 1,
            },
        );
        graph.record(
            ModuleId(1),
            ResolvedBinding {
                symbol: ResolvedSymbolId(2),
                name: BindingName::new("frozen"),
                scope_depth: 0,
                declaration: DeclarationKind::Const,
                reassigned: false,
                read_count: 2,
                write_count: 0,
            },
        );

        // never-reassigned let → upgradeable to const.
        assert!(
            graph
                .module_scope_binding(ModuleId(1), "immutable")
                .expect("immutable present")
                .can_be_const()
        );
        // reassigned var → must stay mutable.
        assert!(
            !graph
                .module_scope_binding(ModuleId(1), "counter")
                .expect("counter present")
                .can_be_const()
        );
        // already const → not an "upgrade".
        assert!(
            !graph
                .module_scope_binding(ModuleId(1), "frozen")
                .expect("frozen present")
                .can_be_const()
        );

        assert_eq!(
            graph
                .module_scope_binding(ModuleId(1), "counter")
                .map(|binding| binding.declaration),
            Some(DeclarationKind::Var),
        );
        assert_eq!(graph.module_scope_binding(ModuleId(1), "absent"), None);
    }

    #[test]
    fn resolved_reference_counts_are_scope_correct() {
        // Two same-named bindings: one only read, one only written. A name-keyed
        // graph would report `x` as both read and written; the resolved graph
        // attributes each to its own symbol.
        let mut graph = ResolvedSymbolGraph::default();
        graph.record(
            ModuleId(1),
            ResolvedBinding {
                symbol: ResolvedSymbolId(0),
                name: BindingName::new("x"),
                scope_depth: 1,
                declaration: DeclarationKind::Const,
                reassigned: false,
                read_count: 3,
                write_count: 0,
            },
        );
        graph.record(
            ModuleId(1),
            ResolvedBinding {
                symbol: ResolvedSymbolId(1),
                name: BindingName::new("x"),
                scope_depth: 1,
                declaration: DeclarationKind::Let,
                reassigned: true,
                read_count: 0,
                write_count: 2,
            },
        );

        let read_only = graph
            .binding(ModuleId(1), ResolvedSymbolId(0))
            .expect("read-only symbol present");
        let write_only = graph
            .binding(ModuleId(1), ResolvedSymbolId(1))
            .expect("write-only symbol present");
        assert_eq!((read_only.read_count, read_only.write_count), (3, 0));
        assert_eq!((write_only.read_count, write_only.write_count), (0, 2));

        // The write-only binding is unread (dead-store candidate); the read-only
        // one is not.
        assert!(write_only.is_unread());
        assert!(!read_only.is_unread());
        let unread = graph.unread_bindings(ModuleId(1));
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].symbol, ResolvedSymbolId(1));
    }

    #[test]
    fn intraprocedural_flow_unreachable_is_sorted_deduped_and_module_scoped() {
        let mut flow = IntraproceduralFlow::default();
        // Inserted out of order, with a duplicate, across two modules.
        flow.add_unreachable(ModuleId(1), ByteRange::new(40, 50));
        flow.add_unreachable(ModuleId(1), ByteRange::new(10, 20));
        flow.add_unreachable(ModuleId(1), ByteRange::new(40, 50));
        flow.add_unreachable(ModuleId(2), ByteRange::new(5, 6));

        assert_eq!(
            flow.unreachable_in(ModuleId(1)),
            &[ByteRange::new(10, 20), ByteRange::new(40, 50)],
        );
        assert!(flow.has_unreachable(ModuleId(1)));
        assert!(!flow.has_unreachable(ModuleId(3)));
        assert_eq!(flow.unreachable_in(ModuleId(3)), &[]);
        assert_eq!(flow.unreachable_count(), 3);
    }

    #[test]
    fn package_surface_does_not_accept_absent_subpath() {
        let surface = PackageSurface::new("lodash").with_root_importable();

        assert!(surface.accepts("lodash"));
        assert!(!surface.accepts("lodash/_mapCacheProto.js"));
    }

    #[test]
    fn write_without_definition_or_import_remains_unresolved() {
        let mut graph = DefUseGraph::default();
        graph.write(ModuleId(1), "missing");

        assert_eq!(graph.unresolved_writes()[0].1.as_str(), "missing");
    }

    #[test]
    fn imported_write_is_resolved() {
        let mut graph = DefUseGraph::default();
        graph.import(ModuleId(1), "namespace");
        graph.write(ModuleId(1), "namespace");

        assert!(graph.unresolved_writes().is_empty());
    }

    #[test]
    fn modules_defining_returns_only_modules_that_define_the_binding() {
        let mut graph = DefUseGraph::default();
        graph.define(ModuleId(1), "helper");
        graph.define(ModuleId(2), "helper");
        graph.import(ModuleId(3), "helper");
        graph.read(ModuleId(4), "helper");

        let owners = graph.modules_defining(&BindingName::new("helper"));

        assert_eq!(
            owners,
            std::collections::BTreeSet::from([ModuleId(1), ModuleId(2)])
        );
    }

    #[test]
    fn data_dependence_edges_connect_resolved_reads_and_writes_to_sources() {
        let mut graph = DefUseGraph::default();
        graph.define(ModuleId(1), "local");
        graph.import(ModuleId(1), "external");
        graph.read(ModuleId(1), "local");
        graph.write(ModuleId(1), "external");
        graph.read(ModuleId(1), "missing");

        let edges = graph.data_dependence_edges();

        assert!(edges.iter().any(|edge| {
            edge.binding.as_str() == "local"
                && edge.source == BindingSourceKind::Definition
                && edge.target == BindingUseKind::Read
        }));
        assert!(edges.iter().any(|edge| {
            edge.binding.as_str() == "external"
                && edge.source == BindingSourceKind::Import
                && edge.target == BindingUseKind::Write
        }));
        assert!(!edges.iter().any(|edge| edge.binding.as_str() == "missing"));
    }

    #[test]
    fn control_flow_graph_records_nodes_and_edges_by_module() {
        let mut graph = ControlFlowGraph::default();
        let entry = graph.add_node(ModuleId(1), ControlFlowNodeKind::Entry);
        let statement = graph.add_node(ModuleId(1), ControlFlowNodeKind::Statement);
        graph.add_edge(
            ModuleId(1),
            entry,
            statement,
            ControlFlowEdgeKind::Sequential,
        );

        assert_eq!(graph.nodes_for(ModuleId(1)).len(), 2);
        assert_eq!(graph.edges_for(ModuleId(1)).len(), 1);
        assert_eq!(
            graph.edges_for(ModuleId(1))[0].kind,
            ControlFlowEdgeKind::Sequential
        );
    }

    #[test]
    fn invalid_uppercase_package_name_is_rejected() {
        assert!(!is_valid_package_name("@smithy/XY7"));
        assert!(!is_valid_package_name("vscode-languageserver-XY7"));
        assert!(is_valid_package_name("@smithy/protocol-http"));
    }

    #[test]
    fn binding_shape_prefers_callable_over_plain_object() {
        assert_eq!(
            BindingShape::PlainObject.merge(BindingShape::Callable),
            BindingShape::Callable
        );
    }

    #[test]
    fn binding_shape_solution_collects_constraints_from_def_use_graph() {
        let mut graph = DefUseGraph::default();
        graph.constrain(BindingConstraint::new(
            ModuleId(1),
            "factory",
            BindingConstraintKind::Call,
        ));
        graph.constrain(BindingConstraint::new(
            ModuleId(1),
            "Service",
            BindingConstraintKind::ClassDeclaration,
        ));

        let solution = BindingShapeSolution::from_def_use_graph(&graph);

        assert_eq!(
            solution.shape_of(ModuleId(1), "factory"),
            BindingShape::Callable
        );
        assert_eq!(
            solution.shape_of(ModuleId(1), "Service"),
            BindingShape::ClassLike
        );
        assert!(solution.conflicts().is_empty());
    }

    #[test]
    fn binding_shape_solution_records_incompatible_constraints() {
        let mut solution = BindingShapeSolution::default();
        solution.add_constraint(&BindingConstraint::new(
            ModuleId(1),
            "NativeModuleType",
            BindingConstraintKind::EnumInitializer,
        ));
        solution.add_constraint(&BindingConstraint::new(
            ModuleId(1),
            "NativeModuleType",
            BindingConstraintKind::Call,
        ));

        assert_eq!(
            solution.shape_of(ModuleId(1), "NativeModuleType"),
            BindingShape::Callable
        );
        assert_eq!(solution.conflicts().len(), 1);
        assert_eq!(
            solution.conflicts()[0].existing_kind,
            BindingConstraintKind::EnumInitializer
        );
        assert_eq!(
            solution.conflicts()[0].incoming_kind,
            BindingConstraintKind::Call
        );
    }

    #[test]
    fn class_declaration_and_construct_constraints_are_compatible() {
        let mut solution = BindingShapeSolution::default();
        solution.add_constraint(&BindingConstraint::new(
            ModuleId(1),
            "Service",
            BindingConstraintKind::ClassDeclaration,
        ));
        solution.add_constraint(&BindingConstraint::new(
            ModuleId(1),
            "Service",
            BindingConstraintKind::Construct,
        ));

        assert_eq!(solution.conflicts(), &[]);
        assert_eq!(
            solution.shape_of(ModuleId(1), "Service"),
            BindingShape::ClassLike
        );
    }

    #[test]
    fn shape_propagates_bidirectionally_through_identity_and_call_aliases() {
        // Identity alias: A = ns. ns starts as PlainObject (object literal
        // declaration); A starts as NamespaceObject (member-read). After
        // alias propagation both should settle at NamespaceObject — same
        // underlying object, max of the lattice.
        let mut graph = DefUseGraph::default();
        graph.constrain(BindingConstraint::new(
            ModuleId(1),
            "ns",
            BindingConstraintKind::ObjectLiteralDeclaration,
        ));
        graph.constrain(BindingConstraint::new(
            ModuleId(1),
            "A",
            BindingConstraintKind::MemberRead,
        ));
        graph.record_identity_alias(ModuleId(1), "A", "ns");

        let solution = BindingShapeSolution::from_def_use_graph(&graph);

        assert_eq!(
            solution.shape_of(ModuleId(1), "A"),
            BindingShape::NamespaceObject,
        );
        assert_eq!(
            solution.shape_of(ModuleId(1), "ns"),
            BindingShape::NamespaceObject,
            "ns should also be upgraded — it's the same underlying object as A",
        );

        // Call-alias chain: B = getNs(), getNs returns ns.
        // B carries no direct constraint; propagation through the return
        // alias should still upgrade B from Unknown to NamespaceObject.
        let mut graph = DefUseGraph::default();
        graph.constrain(BindingConstraint::new(
            ModuleId(1),
            "ns",
            BindingConstraintKind::ObjectLiteralDeclaration,
        ));
        graph.constrain(BindingConstraint::new(
            ModuleId(1),
            "ns",
            BindingConstraintKind::MemberRead,
        ));
        graph.record_call_alias(ModuleId(1), "B", "getNs");
        graph.record_function_return(ModuleId(1), "getNs", "ns");

        let solution = BindingShapeSolution::from_def_use_graph(&graph);
        assert_eq!(
            solution.shape_of(ModuleId(1), "B"),
            BindingShape::NamespaceObject,
            "B should inherit ns's shape via getNs's return alias",
        );
    }

    #[test]
    fn alias_sources_of_closes_over_identity_and_call_return_edges() {
        // Set up: `A = X` (identity), `B = F()` where F returns Y.
        // alias_sources_of(A) should include {A, X};
        // alias_sources_of(B) should include {B, Y};
        // self-reference always present even without edges.
        let mut graph = DefUseGraph::default();
        graph.record_identity_alias(ModuleId(1), "A", "X");
        graph.record_call_alias(ModuleId(1), "B", "F");
        graph.record_function_return(ModuleId(1), "F", "Y");

        let aliases_a = graph.alias_sources_of(ModuleId(1), "A");
        let names_a: Vec<_> = aliases_a.iter().map(BindingName::as_str).collect();
        assert_eq!(names_a, vec!["A", "X"]);

        let aliases_b = graph.alias_sources_of(ModuleId(1), "B");
        let names_b: Vec<_> = aliases_b.iter().map(BindingName::as_str).collect();
        assert_eq!(names_b, vec!["B", "Y"]);

        let aliases_loner = graph.alias_sources_of(ModuleId(1), "no_edges");
        assert_eq!(
            aliases_loner
                .iter()
                .map(BindingName::as_str)
                .collect::<Vec<_>>(),
            vec!["no_edges"],
        );
    }

    #[test]
    fn alias_sources_of_terminates_on_cycle_and_chains_through_transitive_returns() {
        // `A = X`, `X = A` is a cycle — must not loop. `C = F()` where F
        // returns G() and G returns Y should resolve C → {C, Y} after two
        // hops of call-alias composition.
        let mut graph = DefUseGraph::default();
        graph.record_identity_alias(ModuleId(1), "A", "X");
        graph.record_identity_alias(ModuleId(1), "X", "A");

        let aliases = graph.alias_sources_of(ModuleId(1), "A");
        let names: Vec<_> = aliases.iter().map(BindingName::as_str).collect();
        assert_eq!(names, vec!["A", "X"]);

        graph.record_call_alias(ModuleId(1), "C", "F");
        graph.record_call_alias(ModuleId(1), "F_inner", "G");
        graph.record_function_return(ModuleId(1), "F", "F_inner");
        graph.record_function_return(ModuleId(1), "G", "Y");
        let aliases = graph.alias_sources_of(ModuleId(1), "C");
        let names: Vec<_> = aliases.iter().map(BindingName::as_str).collect();
        // Expect closure includes the chain: C → F_inner (returned by F)
        //                                   → Y          (returned by G via F_inner = G())
        assert!(names.contains(&"C"));
        assert!(names.contains(&"F_inner"));
        assert!(names.contains(&"Y"));
    }

    #[test]
    fn def_use_graph_collects_property_names_per_member_access_binding() {
        // Paper #7 (Anderson JS shape) requires constraint-level property
        // tracking so a `NamespaceObject` shape can later carry its known
        // members. The constraint carries an optional property name; the
        // graph aggregates them per (module, binding).
        let mut graph = DefUseGraph::default();
        graph.constrain(
            BindingConstraint::new(ModuleId(1), "ns", BindingConstraintKind::MemberRead)
                .with_property("foo"),
        );
        graph.constrain(
            BindingConstraint::new(ModuleId(1), "ns", BindingConstraintKind::MemberRead)
                .with_property("bar"),
        );
        // Same name twice — must dedup.
        graph.constrain(
            BindingConstraint::new(ModuleId(1), "ns", BindingConstraintKind::MemberRead)
                .with_property("foo"),
        );
        // A MemberRead WITHOUT a property name (older callers) must not
        // pollute the per-binding member set.
        graph.constrain(BindingConstraint::new(
            ModuleId(1),
            "ns",
            BindingConstraintKind::MemberRead,
        ));

        // Same binding name in a different module must be isolated — both
        // modules track their own member set against the `(ModuleId, name)`
        // key, never bleeding across.
        graph.constrain(
            BindingConstraint::new(ModuleId(2), "ns", BindingConstraintKind::MemberRead)
                .with_property("only_in_module_two"),
        );

        let members = graph.members_accessed_on(ModuleId(1), "ns");
        let names: Vec<_> = members.iter().map(BindingName::as_str).collect();
        assert_eq!(names, vec!["bar", "foo"]);

        let module_two_members = graph.members_accessed_on(ModuleId(2), "ns");
        let module_two_names: Vec<_> = module_two_members.iter().map(BindingName::as_str).collect();
        assert_eq!(module_two_names, vec!["only_in_module_two"]);

        // Unknown binding returns an empty set.
        assert!(graph.members_accessed_on(ModuleId(1), "absent").is_empty());
    }

    #[test]
    fn object_literal_declaration_conflicts_with_callable_usage() {
        let mut solution = BindingShapeSolution::default();
        solution.add_constraint(&BindingConstraint::new(
            ModuleId(1),
            "factory",
            BindingConstraintKind::ObjectLiteralDeclaration,
        ));
        solution.add_constraint(&BindingConstraint::new(
            ModuleId(1),
            "factory",
            BindingConstraintKind::Call,
        ));

        assert_eq!(solution.conflicts().len(), 1);
        assert_eq!(
            solution.conflicts()[0].existing_kind,
            BindingConstraintKind::ObjectLiteralDeclaration
        );
        assert_eq!(
            solution.conflicts()[0].incoming_kind,
            BindingConstraintKind::Call
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchTier {
    Exact,
    ExactAlternate,
    StructuralAnchored,
    StructuralAnchoredAlternate,
    FeatureSimilarity,
    FeatureSimilarityAlternate,
    StructuralOnly,
    StructuralOnlyAlternate,
}

impl MatchTier {
    #[must_use]
    pub const fn weight(self) -> u32 {
        match self {
            Self::Exact => 10_000,
            Self::ExactAlternate => 5_000,
            Self::StructuralAnchored => 1_000,
            // Strictly below `StructuralAnchored` so that when both
            // primary and alternate fingerprints offer competing
            // matches for the same (package, fn_id) pair, the
            // Hungarian global assignment prefers the primary one
            // and avoids the ambiguity we observed when both
            // shared the same weight.
            Self::StructuralAnchoredAlternate => 500,
            Self::FeatureSimilarity => 100,
            // Strictly below `FeatureSimilarity` so Hungarian prefers
            // a primary feature-similarity match over an alt-source one
            // when both compete for the same (package, fn_id).
            Self::FeatureSimilarityAlternate => 50,
            Self::StructuralOnly => 10,
            // Strictly below `StructuralOnly` so Hungarian prefers a
            // primary structural-only match over an alt-source one.
            Self::StructuralOnlyAlternate => 5,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::ExactAlternate => "exact_alternate",
            Self::StructuralAnchored => "structural_anchored",
            Self::StructuralAnchoredAlternate => "structural_anchored_alternate",
            Self::FeatureSimilarity => "feature_similarity",
            Self::FeatureSimilarityAlternate => "feature_similarity_alternate",
            Self::StructuralOnly => "structural_only",
            Self::StructuralOnlyAlternate => "structural_only_alternate",
        }
    }
}

#[cfg(test)]
mod match_tier_tests {
    use super::MatchTier;

    #[test]
    fn match_tier_weights_strictly_decrease() {
        let weights = [
            MatchTier::Exact.weight(),
            MatchTier::ExactAlternate.weight(),
            MatchTier::StructuralAnchored.weight(),
            MatchTier::StructuralAnchoredAlternate.weight(),
            MatchTier::FeatureSimilarity.weight(),
            MatchTier::FeatureSimilarityAlternate.weight(),
            MatchTier::StructuralOnly.weight(),
            MatchTier::StructuralOnlyAlternate.weight(),
        ];
        for window in weights.windows(2) {
            assert!(
                window[0] > window[1],
                "tier weights must strictly decrease: {weights:?}"
            );
        }
    }

    #[test]
    fn match_tier_as_str_is_kebab_snake_case() {
        assert_eq!(MatchTier::Exact.as_str(), "exact");
        assert_eq!(MatchTier::ExactAlternate.as_str(), "exact_alternate");
        assert_eq!(
            MatchTier::StructuralAnchored.as_str(),
            "structural_anchored"
        );
        assert_eq!(
            MatchTier::StructuralAnchoredAlternate.as_str(),
            "structural_anchored_alternate"
        );
        assert_eq!(MatchTier::FeatureSimilarity.as_str(), "feature_similarity");
        assert_eq!(
            MatchTier::FeatureSimilarityAlternate.as_str(),
            "feature_similarity_alternate"
        );
        assert_eq!(MatchTier::StructuralOnly.as_str(), "structural_only");
        assert_eq!(
            MatchTier::StructuralOnlyAlternate.as_str(),
            "structural_only_alternate"
        );
    }
}
