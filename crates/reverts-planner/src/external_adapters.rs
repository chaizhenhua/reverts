use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{ModuleDependencyTarget, PackageAttributionInput};
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, read_quoted_string_at as read_quoted_string,
    sanitize_identifier,
};
use reverts_model::EnrichedProgram;
use reverts_package::{
    ExternalImportProof, PackageResolution, PackageSurfaceIndex,
    accepted_external_attribution_for_module, import_attributes_for_attribution,
    parse_export_members_import_proof,
};

use crate::byte_lexer::{find_matching_brace, skip_ws};
use crate::identifiers::{is_identifier_like, parse_identifier};
use crate::source_module_facts::SourceModuleFacts;
use crate::statements::named_export_statement;
use crate::{
    PlannedBinding, PlannedFile, PlannedImport, call_identifiers_in_source, compact_js_source,
    implicit_global_writes_in_source, local_bindings_in_source, previous_non_ws,
    source_exportable_bindings, unique_source_definition_modules,
};

pub(crate) fn adapter_owned_runtime_bindings(
    program: &EnrichedProgram,
    external_package_adapters: &BTreeMap<ModuleId, ExternalPackageAdapterPlan>,
    externalized_packages: &BTreeSet<ModuleId>,
    emitted_bindings: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    if external_package_adapters.is_empty() || emitted_bindings.is_empty() {
        return BTreeSet::new();
    }
    let definition_modules = unique_source_definition_modules(program, externalized_packages);
    // The bindings each adapter actually re-provides: its export bindings plus
    // the members its proof maps from the package surface. A binding whose
    // definition module is an externalized package but which the adapter does
    // NOT re-provide is a package *internal* (e.g. a lazily-assigned default
    // object). It cannot be re-sourced from the bare import, so stripping it
    // from the barrel leaves consumers importing a name nothing exports. Keep
    // such internals inlined; only strip what the adapter genuinely provides.
    let provided_by_module = external_package_adapters
        .iter()
        .map(|(module_id, adapter)| {
            let mut provided = adapter.bindings.clone();
            provided.extend(
                export_member_adapter_binding_map(
                    program,
                    *module_id,
                    adapter.member_proof.as_ref(),
                )
                .into_keys(),
            );
            (*module_id, provided)
        })
        .collect::<BTreeMap<ModuleId, BTreeSet<BindingName>>>();
    emitted_bindings
        .iter()
        .filter(|binding| {
            definition_modules
                .get(*binding)
                .and_then(|module_id| *module_id)
                .and_then(|module_id| provided_by_module.get(&module_id))
                .is_some_and(|provided| provided.contains(*binding))
        })
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalPackageAdapterKind {
    CommonJsWrapper,
    NamespaceReturn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalPackageAdapterPlan {
    pub(crate) bindings: BTreeSet<BindingName>,
    pub(crate) kind: ExternalPackageAdapterKind,
    pub(crate) member_proof: Option<ExportMemberAdapterProof>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportMemberAdapterProofKind {
    BarrelReference,
    BuildVariantPeer,
    CommonJsReexport,
    ExportAllReexport,
    NamedReexport,
    SourceEquivalent,
    Unknown,
}

impl ExportMemberAdapterProofKind {
    fn parse(value: &str) -> Self {
        match value {
            "barrel-reference" => Self::BarrelReference,
            "build-variant-peer" => Self::BuildVariantPeer,
            "commonjs-reexport" => Self::CommonJsReexport,
            "export-all-reexport" => Self::ExportAllReexport,
            "named-reexport" => Self::NamedReexport,
            "source-equivalent" => Self::SourceEquivalent,
            _ => Self::Unknown,
        }
    }

    const fn allows_runtime_alias_side_effect_elision(self) -> bool {
        matches!(self, Self::BuildVariantPeer | Self::SourceEquivalent)
    }

    const fn allows_full_source_replacement(self) -> bool {
        matches!(
            self,
            Self::BuildVariantPeer | Self::CommonJsReexport | Self::SourceEquivalent
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExportMemberAdapterProof {
    pub(crate) kind: ExportMemberAdapterProofKind,
    pub(crate) exported_members: BTreeSet<String>,
    aliases: BTreeMap<BindingName, String>,
}

pub(crate) fn export_member_adapter_proof(
    attribution: &PackageAttributionInput,
) -> Option<ExportMemberAdapterProof> {
    let resolved_file = attribution.resolved_file.as_deref()?;
    let proof = parse_export_members_import_proof(resolved_file)?;
    let aliases = proof
        .aliases
        .into_iter()
        .map(|(local, exported)| (BindingName::new(local), exported))
        .collect();
    Some(ExportMemberAdapterProof {
        kind: ExportMemberAdapterProofKind::parse(proof.proof_kind.as_str()),
        exported_members: proof.exported_members,
        aliases,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalPackageAdapterAnalysis {
    pub(crate) adapters: BTreeMap<ModuleId, ExternalPackageAdapterPlan>,
    pub(crate) adapter_required_packages: BTreeSet<ModuleId>,
}

pub(crate) fn external_package_adapter_analysis(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    source_facts: &SourceModuleFacts,
) -> ExternalPackageAdapterAnalysis {
    let non_empty_call_bindings = non_empty_call_identifiers_in_program(program);

    // Joint fixpoint between adapter-required propagation and the adapter build.
    // A module that is adapter-required but whose adapter cannot be built is
    // *source-preserved* (its real source is kept). A source-preserved consumer
    // is NOT replaced by a bare `import from "<pkg>"`, so its reads of other
    // externalized internals must pin those internals (e.g. vendored lodash
    // `memoize` keeps `new (qF1.Cache || vt)()`, so `vt`/`MapCache` must stay
    // vendored too). But that source-preserved set is only known after building
    // the adapters, which in turn depends on the adapter-required set. Iterate:
    // feed each round's source-preserved modules back as forced read-propagation
    // consumers until the source-preserved set stabilizes. Publicly-importable
    // modules that DO get an adapter (bare import, source dropped) never enter
    // the forced set, so they keep freeing their internals for elimination.
    let mut source_preserved = BTreeSet::<ModuleId>::new();
    loop {
        let adapter_required_packages = adapter_required_package_modules(
            program,
            externalized_packages,
            source_facts,
            &source_preserved,
        );
        let adapters = build_external_package_adapters(
            program,
            source_facts,
            &adapter_required_packages,
            &non_empty_call_bindings,
        );
        let next_source_preserved = adapter_required_packages
            .difference(&adapters.keys().copied().collect())
            .copied()
            .collect::<BTreeSet<_>>();
        if next_source_preserved == source_preserved {
            return ExternalPackageAdapterAnalysis {
                adapters,
                adapter_required_packages,
            };
        }
        source_preserved = next_source_preserved;
    }
}

fn build_external_package_adapters(
    program: &EnrichedProgram,
    source_facts: &SourceModuleFacts,
    adapter_required_packages: &BTreeSet<ModuleId>,
    non_empty_call_bindings: &BTreeSet<BindingName>,
) -> BTreeMap<ModuleId, ExternalPackageAdapterPlan> {
    let adapter_export_bindings =
        package_adapter_export_bindings_by_module(program, source_facts, adapter_required_packages);
    adapter_required_packages
        .iter()
        .filter_map(|module_id| {
            let attribution = accepted_external_attribution_for_module(
                &program.model().input().package_attributions,
                *module_id,
            )?;
            let member_proof = export_member_adapter_proof(attribution);
            let raw_bindings = adapter_export_bindings
                .get(module_id)
                .cloned()
                .unwrap_or_default();
            let kind = external_package_adapter_kind(program, *module_id, &raw_bindings);
            let has_semantic_path_proof =
                external_adapter_attribution_has_semantic_path_proof(attribution);
            if !external_adapter_attribution_allows_eager_import(
                program,
                *module_id,
                attribution,
                kind,
            ) {
                return None;
            }
            if kind == ExternalPackageAdapterKind::CommonJsWrapper
                && member_proof.is_none()
                && !has_semantic_path_proof
                && commonjs_wrapper_source_has_unproven_named_exports(program, *module_id)
            {
                return None;
            }
            let bindings = package_adapter_export_bindings_for_kind(
                program,
                *module_id,
                raw_bindings,
                kind,
                member_proof.as_ref(),
            );
            (has_semantic_path_proof
                || adapter_plan_is_safe(
                    program,
                    *module_id,
                    &bindings,
                    member_proof.as_ref(),
                    non_empty_call_bindings,
                ))
            .then_some((
                *module_id,
                ExternalPackageAdapterPlan {
                    bindings,
                    kind,
                    member_proof,
                },
            ))
        })
        .collect()
}

pub(crate) fn adapter_plan_is_safe(
    program: &EnrichedProgram,
    module_id: ModuleId,
    bindings: &BTreeSet<BindingName>,
    member_proof: Option<&ExportMemberAdapterProof>,
    non_empty_call_bindings: &BTreeSet<BindingName>,
) -> bool {
    let original_name = program
        .model()
        .modules()
        .iter()
        .find(|module| module.id == module_id)
        .map(|module| module.original_name.as_str());
    let member_bindings = export_member_adapter_binding_map(program, module_id, member_proof);
    if adapter_source_has_implicit_side_effect_exports(
        program,
        module_id,
        original_name,
        member_proof,
        &member_bindings,
        bindings,
    ) {
        return false;
    }
    bindings.iter().all(|binding| {
        if Some(binding.as_str()) == original_name {
            return true;
        }
        if member_bindings.contains_key(binding) {
            return true;
        }
        external_adapter_non_original_binding_is_safe(program, module_id, binding)
            && !non_empty_call_bindings.contains(binding)
    })
}

pub(crate) fn adapter_source_has_implicit_side_effect_exports(
    program: &EnrichedProgram,
    module_id: ModuleId,
    original_name: Option<&str>,
    member_proof: Option<&ExportMemberAdapterProof>,
    member_backed_bindings: &BTreeMap<BindingName, String>,
    requested_bindings: &BTreeSet<BindingName>,
) -> bool {
    let Some(source) = program.model().input().module_source_slice(module_id) else {
        return false;
    };
    let compact = compact_js_source(source.source);
    if compact.contains("return_$module.exports;") || compact.contains("module.exports") {
        return false;
    }
    let writes = adapter_relevant_implicit_writes(source.source, member_backed_bindings);
    if adapter_member_proof_allows_full_source_replacement(
        original_name,
        member_proof,
        member_backed_bindings,
        requested_bindings,
    ) {
        return false;
    }
    if let Some(original_name) = original_name
        && requested_bindings.len() == 1
        && requested_bindings
            .iter()
            .all(|binding| binding.as_str() == original_name)
        && source_has_commonjs_wrapper_initializer(compact.as_str(), original_name)
    {
        return false;
    }
    if call_identifiers_in_source(source.source)
        .into_iter()
        .any(|callee| {
            Some(callee.as_str()) != original_name
                && !matches!(
                    callee.as_str(),
                    "E" | "lazyValue" | "lazyModule" | "p" | "__commonJS"
                )
        })
    {
        if adapter_member_proof_allows_call_side_effect_elision(
            compact.as_str(),
            original_name,
            member_proof,
            member_backed_bindings,
            &writes,
        ) {
            return false;
        }
        return true;
    }
    writes.into_iter().any(|binding| {
        Some(binding.as_str()) != original_name && !member_backed_bindings.contains_key(&binding)
    })
}

pub(crate) fn adapter_member_proof_allows_full_source_replacement(
    original_name: Option<&str>,
    member_proof: Option<&ExportMemberAdapterProof>,
    member_backed_bindings: &BTreeMap<BindingName, String>,
    requested_bindings: &BTreeSet<BindingName>,
) -> bool {
    let Some(member_proof) = member_proof else {
        return false;
    };
    if !member_proof.kind.allows_full_source_replacement() || requested_bindings.is_empty() {
        return false;
    }
    requested_bindings.iter().all(|binding| {
        Some(binding.as_str()) == original_name || member_backed_bindings.contains_key(binding)
    })
}

pub(crate) fn adapter_relevant_implicit_writes(
    source: &str,
    member_backed_bindings: &BTreeMap<BindingName, String>,
) -> BTreeSet<BindingName> {
    let local_bindings = local_bindings_in_source(source);
    implicit_global_writes_in_source(source)
        .into_iter()
        .filter(|binding| {
            member_backed_bindings.contains_key(binding)
                || !local_bindings.contains(binding.as_str())
        })
        .collect()
}

pub(crate) fn adapter_member_proof_allows_call_side_effect_elision(
    compact_source: &str,
    original_name: Option<&str>,
    member_proof: Option<&ExportMemberAdapterProof>,
    member_backed_bindings: &BTreeMap<BindingName, String>,
    writes: &BTreeSet<BindingName>,
) -> bool {
    let Some(member_proof) = member_proof else {
        return false;
    };
    if !member_proof.kind.allows_runtime_alias_side_effect_elision()
        || member_backed_bindings.is_empty()
        || writes.is_empty()
    {
        return false;
    }
    if writes.iter().any(|binding| {
        Some(binding.as_str()) != original_name && !member_backed_bindings.contains_key(binding)
    }) {
        return false;
    }
    let Some(original_name) = original_name else {
        return false;
    };
    source_has_adapter_lazy_initializer(compact_source, original_name)
}

pub(crate) fn source_has_adapter_lazy_initializer(
    compact_source: &str,
    original_name: &str,
) -> bool {
    [
        format!("var{original_name}=E("),
        format!("let{original_name}=E("),
        format!("const{original_name}=E("),
        format!("var{original_name}=lazyValue("),
        format!("let{original_name}=lazyValue("),
        format!("const{original_name}=lazyValue("),
        format!("var{original_name}=lazyModule("),
        format!("let{original_name}=lazyModule("),
        format!("const{original_name}=lazyModule("),
        format!("var{original_name}=__commonJS("),
        format!("let{original_name}=__commonJS("),
        format!("const{original_name}=__commonJS("),
    ]
    .iter()
    .any(|needle| compact_source.contains(needle))
}

pub(crate) fn source_has_commonjs_wrapper_initializer(
    compact_source: &str,
    original_name: &str,
) -> bool {
    [
        format!("var{original_name}=p("),
        format!("let{original_name}=p("),
        format!("const{original_name}=p("),
        format!("var{original_name}=__commonJS("),
        format!("let{original_name}=__commonJS("),
        format!("const{original_name}=__commonJS("),
        format!("var{original_name}=U(("),
        format!("let{original_name}=U(("),
        format!("const{original_name}=U(("),
    ]
    .iter()
    .any(|needle| compact_source.contains(needle))
        && compact_source.contains(".exports")
}

pub(crate) fn commonjs_wrapper_source_has_unproven_named_exports(
    program: &EnrichedProgram,
    module_id: ModuleId,
) -> bool {
    let Some(source) = program.model().input().module_source_slice(module_id) else {
        return false;
    };
    commonjs_wrapper_compact_source_has_named_exports(compact_js_source(source.source).as_str())
}

pub(crate) fn commonjs_wrapper_compact_source_has_named_exports(compact_source: &str) -> bool {
    compact_source.contains("exports.") || compact_source.contains(".exports={")
}

pub(crate) fn external_adapter_non_original_binding_is_safe(
    program: &EnrichedProgram,
    module_id: ModuleId,
    binding: &BindingName,
) -> bool {
    let Some(source) = program.model().input().module_source_slice(module_id) else {
        return false;
    };
    let compact = compact_js_source(source.source);
    compact_source_declares_adapter_safe_binding(compact.as_str(), binding.as_str())
}

pub(crate) fn export_member_adapter_binding_map(
    program: &EnrichedProgram,
    module_id: ModuleId,
    member_proof: Option<&ExportMemberAdapterProof>,
) -> BTreeMap<BindingName, String> {
    let Some(member_proof) = member_proof else {
        return BTreeMap::new();
    };
    let mut bindings = member_proof
        .exported_members
        .iter()
        .filter(|member| is_identifier_like(member.as_str()))
        .map(|member| (BindingName::new(member.clone()), member.clone()))
        .collect::<BTreeMap<_, _>>();
    for (local, exported) in &member_proof.aliases {
        if member_proof.exported_members.contains(exported.as_str()) {
            bindings.insert(local.clone(), exported.clone());
        }
    }
    let Some(source) = program.model().input().module_source_slice(module_id) else {
        return bindings;
    };
    for (local, exported) in commonjs_exported_member_bindings(source.source) {
        if member_proof.exported_members.contains(exported.as_str()) {
            bindings.insert(local, exported);
        }
    }
    for (local, exported) in
        class_runtime_name_member_bindings(source.source, &member_proof.exported_members)
    {
        bindings.entry(local).or_insert(exported);
    }
    bindings
}

pub(crate) fn commonjs_exported_member_bindings(source: &str) -> BTreeMap<BindingName, String> {
    let compact = compact_js_source(source);
    let mut bindings = BTreeMap::new();
    collect_direct_member_export_assignments(compact.as_str(), &mut bindings);
    collect_define_property_member_getters(compact.as_str(), &mut bindings);
    bindings
}

pub(crate) fn class_runtime_name_member_bindings(
    source: &str,
    exported_members: &BTreeSet<String>,
) -> BTreeMap<BindingName, String> {
    let mut bindings = BTreeMap::new();
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("class") {
        let class_start = cursor + relative;
        if !keyword_boundary(source, class_start, "class") {
            cursor = class_start + "class".len();
            continue;
        }
        let Some((local, class_body_end)) = class_local_binding_and_body_end(source, class_start)
        else {
            cursor = class_start + "class".len();
            continue;
        };
        let class_source = &source[class_start..=class_body_end.min(source.len() - 1)];
        let compact_class = compact_js_source(class_source);
        for member in exported_members {
            if class_declares_runtime_name(compact_class.as_str(), member.as_str()) {
                bindings.insert(local.clone(), member.clone());
                break;
            }
        }
        cursor = class_body_end.saturating_add(1);
    }
    bindings
}

pub(crate) fn keyword_boundary(source: &str, start: usize, keyword: &str) -> bool {
    let end = start + keyword.len();
    let bytes = source.as_bytes();
    let before_ok = start == 0 || !is_identifier_continue(bytes[start - 1]);
    let after_ok = bytes
        .get(end)
        .is_none_or(|byte| !is_identifier_continue(*byte));
    before_ok && after_ok
}

pub(crate) fn class_local_binding_and_body_end(
    source: &str,
    class_start: usize,
) -> Option<(BindingName, usize)> {
    let bytes = source.as_bytes();
    let after_keyword = skip_ws(bytes, class_start + "class".len());
    let (class_name, after_class_name) = parse_identifier(source, after_keyword)?;
    let local = assigned_identifier_before_expression(source, class_start)
        .unwrap_or_else(|| BindingName::new(class_name));
    let body_start = source[after_class_name..]
        .find('{')
        .map(|relative| after_class_name + relative)?;
    let body_end = find_matching_brace(source, body_start)?;
    Some((local, body_end))
}

pub(crate) fn assigned_identifier_before_expression(
    source: &str,
    expression_start: usize,
) -> Option<BindingName> {
    let bytes = source.as_bytes();
    let equals = previous_non_ws(bytes, expression_start)?;
    if bytes.get(equals) != Some(&b'=') {
        return None;
    }
    let end = previous_non_ws(bytes, equals)?;
    let mut start = end;
    while start > 0 && is_identifier_continue(bytes[start - 1]) {
        start -= 1;
    }
    if start > end || !is_identifier_start(bytes[start]) {
        return None;
    }
    let name = &source[start..=end];
    is_identifier_like(name).then(|| BindingName::new(name))
}

pub(crate) fn class_declares_runtime_name(compact_class_source: &str, member: &str) -> bool {
    compact_class_source.contains(format!("name=\"{member}\"").as_str())
        || compact_class_source.contains(format!("name='{member}'").as_str())
}

pub(crate) fn collect_direct_member_export_assignments(
    compact_source: &str,
    bindings: &mut BTreeMap<BindingName, String>,
) {
    let bytes = compact_source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some(dot) = compact_source[cursor..].find('.') else {
            return;
        };
        let member_start = cursor + dot + 1;
        let Some((exported, after_member)) = parse_identifier(compact_source, member_start) else {
            cursor = member_start;
            continue;
        };
        if bytes.get(after_member) != Some(&b'=') {
            cursor = after_member;
            continue;
        }
        let Some((local, after_local)) = parse_identifier(compact_source, after_member + 1) else {
            cursor = after_member + 1;
            continue;
        };
        if is_identifier_like(exported) && is_identifier_like(local) {
            bindings.insert(BindingName::new(local), exported.to_string());
        }
        cursor = after_local;
    }
}

pub(crate) fn collect_define_property_member_getters(
    compact_source: &str,
    bindings: &mut BTreeMap<BindingName, String>,
) {
    let needle = "Object.defineProperty(";
    let mut cursor = 0;
    while let Some(relative) = compact_source[cursor..].find(needle) {
        let start = cursor + relative + needle.len();
        let Some(first_comma) = compact_source[start..].find(',').map(|index| start + index) else {
            return;
        };
        let member_start = first_comma + 1;
        let Some((exported, after_exported)) = read_quoted_string(compact_source, member_start)
        else {
            cursor = member_start;
            continue;
        };
        let Some(return_index) = compact_source[after_exported..]
            .find("return")
            .map(|index| after_exported + index + "return".len())
        else {
            cursor = after_exported;
            continue;
        };
        let Some((local, after_local)) = parse_identifier(compact_source, return_index) else {
            cursor = return_index;
            continue;
        };
        if is_identifier_like(exported.as_str()) && is_identifier_like(local) {
            bindings.insert(BindingName::new(local), exported);
        }
        cursor = after_local;
    }
}

pub(crate) fn compact_source_declares_adapter_safe_binding(
    compact_source: &str,
    binding: &str,
) -> bool {
    [
        format!("function{binding}("),
        format!("asyncfunction{binding}("),
        format!("var{binding}=function"),
        format!("let{binding}=function"),
        format!("const{binding}=function"),
        format!("var{binding}=asyncfunction"),
        format!("let{binding}=asyncfunction"),
        format!("const{binding}=asyncfunction"),
        format!("var{binding}=()=>"),
        format!("let{binding}=()=>"),
        format!("const{binding}=()=>"),
        format!("var{binding}=(()=>"),
        format!("let{binding}=(()=>"),
        format!("const{binding}=(()=>"),
        format!("var{binding}=E("),
        format!("let{binding}=E("),
        format!("const{binding}=E("),
        format!("var{binding}=__commonJS("),
        format!("let{binding}=__commonJS("),
        format!("const{binding}=__commonJS("),
        format!("var{binding}=lazyValue("),
        format!("let{binding}=lazyValue("),
        format!("const{binding}=lazyValue("),
        format!("var{binding}=lazyModule("),
        format!("let{binding}=lazyModule("),
        format!("const{binding}=lazyModule("),
        format!("var{binding}=_$l("),
        format!("let{binding}=_$l("),
        format!("const{binding}=_$l("),
        format!("var{binding}={{"),
        format!("let{binding}={{"),
        format!("const{binding}={{"),
        format!("var{binding}=Object.freeze({{"),
        format!("let{binding}=Object.freeze({{"),
        format!("const{binding}=Object.freeze({{"),
    ]
    .iter()
    .any(|needle| compact_source.contains(needle))
}

pub(crate) fn non_empty_call_identifiers_in_program(
    program: &EnrichedProgram,
) -> BTreeSet<BindingName> {
    let mut bindings = BTreeSet::new();
    for source in program
        .model()
        .input()
        .source_files
        .iter()
        .filter_map(|source_file| source_file.source.as_deref())
    {
        bindings.extend(non_empty_call_identifiers_in_source(source));
    }
    bindings
}

pub(crate) fn non_empty_call_identifiers_in_source(source: &str) -> BTreeSet<BindingName> {
    let bytes = source.as_bytes();
    let mut bindings = BTreeSet::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if !is_identifier_start(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        let Some((identifier, end)) = parse_identifier(source, cursor) else {
            cursor += 1;
            continue;
        };
        let after = skip_ws(bytes, end);
        if bytes.get(after) == Some(&b'(') {
            let inner = skip_ws(bytes, after + 1);
            if bytes.get(inner) != Some(&b')') {
                bindings.insert(BindingName::new(identifier));
            }
        }
        cursor = end;
    }
    bindings
}

pub(crate) fn external_package_adapter_kind(
    program: &EnrichedProgram,
    module_id: ModuleId,
    adapter_bindings: &BTreeSet<BindingName>,
) -> ExternalPackageAdapterKind {
    let Some(source) = program.model().input().module_source_slice(module_id) else {
        return ExternalPackageAdapterKind::NamespaceReturn;
    };
    let compact = compact_js_source(source.source);
    if compact.contains("let_$cached;") && compact.contains("return_$module.exports;") {
        return ExternalPackageAdapterKind::CommonJsWrapper;
    }
    if adapter_bindings
        .iter()
        .any(|binding| source_has_commonjs_wrapper_initializer(compact.as_str(), binding.as_str()))
    {
        return ExternalPackageAdapterKind::CommonJsWrapper;
    }
    ExternalPackageAdapterKind::NamespaceReturn
}

pub(crate) fn populate_external_package_adapter_file(
    file: &mut PlannedFile,
    program: &EnrichedProgram,
    module_id: ModuleId,
    attribution: &PackageAttributionInput,
    exportable_bindings: &BTreeSet<BindingName>,
    adapter_kind: ExternalPackageAdapterKind,
    member_proof: Option<&ExportMemberAdapterProof>,
) {
    let Some(specifier) = attribution.export_specifier.as_deref() else {
        return;
    };
    let namespace = external_package_adapter_namespace(attribution, exportable_bindings);
    file.add_import(PlannedImport {
        namespace: namespace.clone(),
        resolution: PackageResolution::External {
            specifier: specifier.to_string(),
            package_name: attribution.package_name.clone(),
            import_attributes: import_attributes_for_attribution(attribution),
        },
        source_backed: false,
    });
    let return_expression =
        external_package_adapter_return_expression(namespace.as_str(), adapter_kind);
    let namespace_expression = namespace.as_str().to_string();
    let member_bindings = export_member_adapter_binding_map(program, module_id, member_proof);
    for binding in exportable_bindings {
        let adapter_binding_kind =
            external_package_adapter_binding_kind(program, module_id, binding);
        if !external_package_adapter_binding_is_original(program, module_id, binding)
            && let Some(exported_member) = member_bindings.get(binding)
        {
            file.push_source(format!(
                "const {} = {};",
                binding.as_str(),
                external_package_adapter_member_expression(
                    namespace.as_str(),
                    adapter_kind,
                    exported_member.as_str()
                )
            ));
        } else {
            match adapter_binding_kind {
                ExternalPackageAdapterBindingKind::Callable => {
                    file.push_source(format!(
                        "function {}() {{ return {}; }}",
                        binding.as_str(),
                        return_expression
                    ));
                }
                ExternalPackageAdapterBindingKind::Value => {
                    file.push_source(format!(
                        "const {} = {};",
                        binding.as_str(),
                        namespace_expression
                    ));
                }
            }
        }
        file.add_binding(PlannedBinding::new(
            binding.clone(),
            binding.clone(),
            adapter_binding_kind.binding_shape(),
            true,
        ));
    }
    file.push_source(named_export_statement(exportable_bindings.iter()));
    for binding in exportable_bindings {
        file.add_export_with_source_backed(binding.clone(), true);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalPackageAdapterBindingKind {
    Callable,
    Value,
}

impl ExternalPackageAdapterBindingKind {
    const fn binding_shape(self) -> BindingShape {
        match self {
            Self::Callable => BindingShape::Callable,
            Self::Value => BindingShape::NamespaceObject,
        }
    }
}

pub(crate) fn external_package_adapter_binding_kind(
    program: &EnrichedProgram,
    module_id: ModuleId,
    binding: &BindingName,
) -> ExternalPackageAdapterBindingKind {
    if external_package_adapter_binding_is_original(program, module_id, binding) {
        return ExternalPackageAdapterBindingKind::Callable;
    }
    match program.binding_shape(module_id, binding.as_str()) {
        BindingShape::Callable | BindingShape::Constructor | BindingShape::ClassLike => {
            return ExternalPackageAdapterBindingKind::Callable;
        }
        BindingShape::Unknown
        | BindingShape::Value
        | BindingShape::PlainObject
        | BindingShape::NamespaceObject
        | BindingShape::EnumObject => {}
    }
    if external_package_source_defines_callable_binding(program, module_id, binding) {
        return ExternalPackageAdapterBindingKind::Callable;
    }
    ExternalPackageAdapterBindingKind::Value
}

pub(crate) fn external_package_adapter_binding_is_original(
    program: &EnrichedProgram,
    module_id: ModuleId,
    binding: &BindingName,
) -> bool {
    program
        .model()
        .modules()
        .iter()
        .find(|module| module.id == module_id)
        .is_some_and(|module| module.original_name == binding.as_str())
}

pub(crate) fn external_package_source_defines_callable_binding(
    program: &EnrichedProgram,
    module_id: ModuleId,
    binding: &BindingName,
) -> bool {
    let Some(source) = program.model().input().module_source_slice(module_id) else {
        return false;
    };
    compact_source_defines_callable_binding(
        compact_js_source(source.source).as_str(),
        binding.as_str(),
    )
}

pub(crate) fn compact_source_defines_callable_binding(compact_source: &str, binding: &str) -> bool {
    [
        format!("function{binding}("),
        format!("asyncfunction{binding}("),
        format!("var{binding}=function"),
        format!("let{binding}=function"),
        format!("const{binding}=function"),
        format!("var{binding}=asyncfunction"),
        format!("let{binding}=asyncfunction"),
        format!("const{binding}=asyncfunction"),
        format!("var{binding}=()=>"),
        format!("let{binding}=()=>"),
        format!("const{binding}=()=>"),
        format!("var{binding}=(()=>"),
        format!("let{binding}=(()=>"),
        format!("const{binding}=(()=>"),
        format!("var{binding}=async()=>"),
        format!("let{binding}=async()=>"),
        format!("const{binding}=async()=>"),
        format!("var{binding}=(async()=>"),
        format!("let{binding}=(async()=>"),
        format!("const{binding}=(async()=>"),
        format!("var{binding}=lazyValue("),
        format!("let{binding}=lazyValue("),
        format!("const{binding}=lazyValue("),
        format!("var{binding}=lazyModule("),
        format!("let{binding}=lazyModule("),
        format!("const{binding}=lazyModule("),
        format!("var{binding}=E("),
        format!("let{binding}=E("),
        format!("const{binding}=E("),
        format!("var{binding}=__commonJS("),
        format!("let{binding}=__commonJS("),
        format!("const{binding}=__commonJS("),
        format!("var{binding}=p("),
        format!("let{binding}=p("),
        format!("const{binding}=p("),
    ]
    .iter()
    .any(|needle| compact_source.contains(needle))
        || compact_source_defines_thunk_factory_binding(compact_source, binding)
}

pub(crate) fn compact_source_defines_thunk_factory_binding(
    compact_source: &str,
    binding: &str,
) -> bool {
    ["var", "let", "const"].iter().any(|declaration| {
        let needle = format!("{declaration}{binding}=");
        let mut search_start = 0usize;
        while let Some(relative) = compact_source[search_start..].find(needle.as_str()) {
            let initializer_start = search_start + relative + needle.len();
            if compact_initializer_is_thunk_factory(&compact_source[initializer_start..]) {
                return true;
            }
            search_start = initializer_start;
        }
        false
    })
}

pub(crate) fn compact_initializer_is_thunk_factory(source: &str) -> bool {
    let Some((callee, callee_end)) = parse_identifier(source, 0) else {
        return false;
    };
    if callee.is_empty() || source.as_bytes().get(callee_end) != Some(&b'(') {
        return false;
    }
    let argument = &source[callee_end + 1..];
    argument.starts_with("()=>")
        || argument.starts_with("async()=>")
        || argument.starts_with("function(")
        || argument.starts_with("asyncfunction(")
}

pub(crate) fn external_package_adapter_return_expression(
    namespace: &str,
    adapter_kind: ExternalPackageAdapterKind,
) -> String {
    match adapter_kind {
        ExternalPackageAdapterKind::CommonJsWrapper => format!(
            "Object.prototype.hasOwnProperty.call({namespace}, \"default\") ? {namespace}.default : {namespace}"
        ),
        ExternalPackageAdapterKind::NamespaceReturn => namespace.to_string(),
    }
}

pub(crate) fn external_package_adapter_member_expression(
    namespace: &str,
    adapter_kind: ExternalPackageAdapterKind,
    member: &str,
) -> String {
    let object = external_package_adapter_return_expression(namespace, adapter_kind);
    if object == namespace {
        format!("{namespace}.{member}")
    } else {
        format!("({object}).{member}")
    }
}

pub(crate) fn package_adapter_export_bindings_by_module(
    program: &EnrichedProgram,
    source_facts: &SourceModuleFacts,
    adapter_required_packages: &BTreeSet<ModuleId>,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let mut bindings_by_module = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    let mut target_bindings_by_module = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    for module_id in adapter_required_packages {
        let target_bindings = source_facts
            .exportable_bindings_by_module
            .get(module_id)
            .cloned()
            .unwrap_or_else(|| source_exportable_bindings(program, *module_id));
        target_bindings_by_module.insert(*module_id, target_bindings);
    }
    for dependency in &program.model().input().dependencies {
        let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
            continue;
        };
        let Some(target_bindings) = target_bindings_by_module.get(&target_module_id) else {
            continue;
        };
        let Some(candidate_reads) = source_facts
            .candidate_reads_by_module
            .get(&dependency.from_module_id)
        else {
            continue;
        };
        bindings_by_module
            .entry(target_module_id)
            .or_default()
            .extend(candidate_reads.intersection(target_bindings).cloned());
    }
    for (from_module_id, candidate_reads) in &source_facts.candidate_reads_by_module {
        for binding in candidate_reads {
            let Some(owner) = source_facts
                .definition_modules_all
                .get(binding)
                .and_then(|owner| *owner)
            else {
                continue;
            };
            if owner == *from_module_id || !adapter_required_packages.contains(&owner) {
                continue;
            }
            if target_bindings_by_module
                .get(&owner)
                .is_some_and(|target_bindings| !target_bindings.contains(binding))
            {
                continue;
            }
            bindings_by_module
                .entry(owner)
                .or_default()
                .insert(binding.clone());
        }
    }
    bindings_by_module
}

pub(crate) fn package_adapter_export_bindings_for_kind(
    program: &EnrichedProgram,
    module_id: ModuleId,
    mut bindings: BTreeSet<BindingName>,
    adapter_kind: ExternalPackageAdapterKind,
    member_proof: Option<&ExportMemberAdapterProof>,
) -> BTreeSet<BindingName> {
    if adapter_kind != ExternalPackageAdapterKind::CommonJsWrapper || member_proof.is_some() {
        return bindings;
    }
    let Some(original) = program
        .model()
        .modules()
        .iter()
        .find(|module| module.id == module_id)
        .map(|module| BindingName::new(module.original_name.clone()))
    else {
        return bindings;
    };
    if !bindings.contains(&original) {
        return bindings;
    }
    bindings.retain(|binding| binding == &original);
    bindings
}

pub(crate) fn external_adapter_attribution_allows_eager_import(
    program: &EnrichedProgram,
    module_id: ModuleId,
    attribution: &PackageAttributionInput,
    adapter_kind: ExternalPackageAdapterKind,
) -> bool {
    let Some(resolved_file) = attribution.resolved_file.as_deref() else {
        return false;
    };
    let not_worker = [
        attribution.export_specifier.as_deref(),
        attribution.resolved_file.as_deref(),
        attribution.subpath.as_deref(),
    ]
    .into_iter()
    .flatten()
    .all(|value| !external_adapter_specifier_is_worker_asset(value));
    if !not_worker {
        return false;
    }
    if !external_adapter_resolved_file_has_weak_source_replacement_proof(resolved_file) {
        return true;
    }
    adapter_kind == ExternalPackageAdapterKind::CommonJsWrapper
        && external_adapter_has_no_module_dependencies(program, module_id)
        && attribution_matches_self_contained_subpath_identity(program, module_id, attribution)
}

fn external_adapter_specifier_is_worker_asset(value: &str) -> bool {
    value.to_ascii_lowercase().contains(".worker")
}

fn external_adapter_attribution_has_semantic_path_proof(
    attribution: &PackageAttributionInput,
) -> bool {
    attribution
        .resolved_file
        .as_deref()
        .is_some_and(|value| ExternalImportProof::parse(value).is_semantic_path_proof())
}

fn external_adapter_resolved_file_has_weak_source_replacement_proof(value: &str) -> bool {
    ExternalImportProof::parse(value).is_weak_source_replacement_proof()
}

fn external_adapter_has_no_module_dependencies(
    program: &EnrichedProgram,
    module_id: ModuleId,
) -> bool {
    program
        .model()
        .input()
        .dependencies
        .iter()
        .filter(|dependency| dependency.from_module_id == module_id)
        .all(|dependency| !matches!(dependency.target, ModuleDependencyTarget::Module(_)))
}

fn attribution_matches_self_contained_subpath_identity(
    program: &EnrichedProgram,
    module_id: ModuleId,
    attribution: &PackageAttributionInput,
) -> bool {
    let Some(source_path) = attribution
        .resolved_file
        .as_deref()
        .or(attribution.export_specifier.as_deref())
    else {
        return false;
    };
    let source_identity = SourceSubpathIdentity::from_resolved_source_path(
        source_path,
        attribution.package_name.as_str(),
        attribution.package_version.as_deref(),
    );
    let Some(source_identity) = source_identity.filter(|identity| identity.is_unambiguous_leaf())
    else {
        return false;
    };
    program
        .model()
        .modules()
        .iter()
        .find(|module| module.id == module_id)
        .is_some_and(|module| source_identity.matches_module_semantic_path(&module.semantic_path))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceSubpathIdentity {
    leaf_stem: String,
    parent_segments: usize,
}

impl SourceSubpathIdentity {
    fn from_resolved_source_path(
        source_path: &str,
        package_name: &str,
        package_version: Option<&str>,
    ) -> Option<Self> {
        let relative_path = external_adapter_package_relative_source_path(
            source_path,
            package_name,
            package_version,
        )?;
        let segments = relative_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        let file_name = segments.last()?;
        let leaf_stem = file_name.split('.').next()?.trim();
        if leaf_stem.is_empty() {
            return None;
        }
        Some(Self {
            leaf_stem: normalize_subpath_identity_segment(leaf_stem),
            parent_segments: segments.len().saturating_sub(1),
        })
    }

    fn is_unambiguous_leaf(&self) -> bool {
        !matches!(
            self.leaf_stem.as_str(),
            "index" | "main" | "module" | "browser" | "cjs" | "esm" | "umd"
        ) && self.parent_segments > 0
    }

    fn matches_module_semantic_path(&self, semantic_path: &str) -> bool {
        semantic_path
            .split('/')
            .filter_map(|segment| segment.split('.').next())
            .map(normalize_subpath_identity_segment)
            .any(|segment| {
                segment == self.leaf_stem
                    || (self.leaf_stem.len() >= 3 && segment.contains(&self.leaf_stem))
            })
    }
}

fn external_adapter_package_relative_source_path<'a>(
    source_path: &'a str,
    package_name: &str,
    package_version: Option<&str>,
) -> Option<&'a str> {
    let trimmed = source_path
        .rsplit(':')
        .next()
        .unwrap_or(source_path)
        .trim()
        .trim_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if let Some(version) = package_version
        && let Some(relative) = trimmed
            .strip_prefix(format!("{package_name}@{version}/").as_str())
            .filter(|relative| !relative.is_empty())
    {
        return Some(relative);
    }
    trimmed
        .strip_prefix(format!("{package_name}/").as_str())
        .filter(|relative| !relative.is_empty())
}

fn normalize_subpath_identity_segment(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub(crate) fn external_package_adapter_namespace(
    attribution: &PackageAttributionInput,
    exportable_bindings: &BTreeSet<BindingName>,
) -> BindingName {
    let sanitized = sanitize_identifier(attribution.package_name.as_str());
    let base = if sanitized.is_empty() {
        "externalPackage".to_string()
    } else {
        format!("external_{sanitized}")
    };
    if !exportable_bindings
        .iter()
        .any(|binding| binding.as_str() == base.as_str())
    {
        return BindingName::new(base);
    }
    BindingName::new(format!("{base}Namespace"))
}

pub(crate) fn adapter_required_package_modules(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    source_facts: &SourceModuleFacts,
    source_preserved_consumers: &BTreeSet<ModuleId>,
) -> BTreeSet<ModuleId> {
    let candidate_reads_by_module = &source_facts.candidate_reads_by_module;
    let definition_modules = &source_facts.definition_modules_all;
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let exportable_bindings_by_module = &source_facts.exportable_bindings_by_module;
    let mut required = BTreeSet::new();

    // A module that is publicly importable (its specifier is accepted by the
    // package surfaces derived from each package's real public API) is replaced
    // in the output by a bare `import from "<package>"`. Such a module must not
    // propagate adapter-required-ness to its internal dependencies: the import
    // already provides them, so those internals become eliminable. Internals
    // with a direct application consumer are still pinned through that consumer.
    // The exception is a publicly-importable module that is itself known to be
    // source-preserved (no adapter could be built for it): its real source is
    // kept and still references those internals, so it must keep propagating.
    let public_surface_index = PackageSurfaceIndex::from_attributions(
        &[],
        program.model().input().package_surfaces.as_slice(),
    );
    let publicly_importable = externalized_packages
        .iter()
        .copied()
        .filter(|module_id| {
            accepted_external_attribution_for_module(
                &program.model().input().package_attributions,
                *module_id,
            )
            .and_then(|attribution| attribution.export_specifier.as_deref())
            .is_some_and(|specifier| {
                matches!(
                    public_surface_index.resolve(specifier),
                    PackageResolution::External { .. }
                )
            })
        })
        .collect::<BTreeSet<ModuleId>>();

    loop {
        let mut changed = false;
        for dependency in &program.model().input().dependencies {
            let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
                continue;
            };
            if !externalized_packages.contains(&target_module_id)
                || required.contains(&target_module_id)
            {
                continue;
            }
            let Some(from_module) = modules_by_id.get(&dependency.from_module_id) else {
                continue;
            };
            if publicly_importable.contains(&dependency.from_module_id)
                && !source_preserved_consumers.contains(&dependency.from_module_id)
            {
                continue;
            }
            if from_module.kind == ModuleKind::Package
                && externalized_packages.contains(&from_module.id)
                && !required.contains(&from_module.id)
            {
                continue;
            }
            let Some(candidate_reads) = candidate_reads_by_module.get(&dependency.from_module_id)
            else {
                continue;
            };
            let Some(target_bindings) = exportable_bindings_by_module.get(&target_module_id) else {
                continue;
            };
            if candidate_reads.is_disjoint(target_bindings) {
                continue;
            }
            required.insert(target_module_id);
            changed = true;
        }
        for (from_module_id, candidate_reads) in candidate_reads_by_module {
            let Some(from_module) = modules_by_id.get(from_module_id) else {
                continue;
            };
            if publicly_importable.contains(from_module_id)
                && !source_preserved_consumers.contains(from_module_id)
            {
                continue;
            }
            if from_module.kind == ModuleKind::Package
                && externalized_packages.contains(&from_module.id)
                && !required.contains(&from_module.id)
            {
                continue;
            }
            for binding in candidate_reads {
                let Some(Some(target_module_id)) = definition_modules.get(binding) else {
                    continue;
                };
                if target_module_id == from_module_id
                    || !externalized_packages.contains(target_module_id)
                    || required.contains(target_module_id)
                {
                    continue;
                }
                required.insert(*target_module_id);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    required
}
