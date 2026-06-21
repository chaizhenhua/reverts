pub mod rollup;

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::{AstFactKind, AstWrapperKind, FunctionExtractor};
use reverts_input::{ModuleDependencyInput, ModuleDependencyTarget, SymbolScope};
use reverts_ir::{
    BindingConstraintKind, BindingName, BindingShapeSolution, ControlFlowEdgeKind,
    ControlFlowNodeKind, FunctionFingerprint, InferredType, ModuleId, TypeSolution,
};
use reverts_js::{
    GeneratedTypeKind, ParseGoal, collect_identifier_read_facts,
    collect_top_level_literal_type_annotations, is_generated_placeholder_identifier,
    sanitize_identifier,
};
pub use reverts_model::CompilerKind;
use reverts_model::{
    CompilerEvidence, CompilerProfile, EnrichedProgram, ModuleCompilerProfile,
    PackageImportDecision, ProgramModel, SemanticNameMap,
};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::{PackageResolution, PackageSurfaceIndex};

#[derive(Debug, Clone, PartialEq)]
pub struct EnrichmentOutput {
    pub program: EnrichedProgram,
    pub audit: AuditReport,
}

#[must_use]
pub fn enrich_program(mut model: ProgramModel) -> EnrichmentOutput {
    // Wire cross-module free reads to their owning module. Bundle slicing
    // (esp. esbuild scope-hoisting) leaves a module reading a binding that is
    // defined in a sibling module with no explicit dependency edge; without
    // the edge the planner cannot emit the import. Resolve only unambiguous
    // single-owner reads — never guess for a colliding name.
    let synthesized = synthesize_module_dependency_edges(&model);
    model.register_module_imports(synthesized.imports);
    model.add_module_dependencies(synthesized.edges);

    let semantic_names = assign_semantic_names(&model);
    let binding_shapes = BindingShapeSolution::from_def_use_graph(model.graph().def_use());
    let type_solution = infer_type_solution(&model);
    let compiler_profile = detect_compiler_profile(&model);
    let package_index = PackageSurfaceIndex::from_attributions(
        model.input().package_attributions.as_slice(),
        model.input().package_surfaces.as_slice(),
    );
    let mut audit = AuditReport::default();
    audit.extend(audit_ast_fact_extraction(&model));
    audit.extend(audit_def_use_graph(&model));
    audit.extend(audit_binding_shape_conflicts(&binding_shapes));
    audit.extend(audit_unprotected_nullable_member_reads(&model));
    audit.extend(audit_unreachable_top_level_code(&model));
    audit.extend(audit_unreachable_function_code(&model));
    let package_imports = resolve_package_imports(&model, &package_index, &mut audit);

    let mut function_fingerprints: BTreeMap<ModuleId, Vec<FunctionFingerprint>> = BTreeMap::new();
    if should_collect_enrichment_function_fingerprints(&model) {
        for module in model.modules() {
            if let Some(slice) = model.input().module_source_slice(module.id) {
                let fps = FunctionExtractor::fingerprint(module.id, slice.source);
                if !fps.is_empty() {
                    function_fingerprints.insert(module.id, fps);
                }
            }
        }
    }

    EnrichmentOutput {
        program: EnrichedProgram::new(model, semantic_names, package_imports, binding_shapes)
            .with_type_solution(type_solution)
            .with_compiler_profile(compiler_profile)
            .with_function_fingerprints(function_fingerprints),
        audit,
    }
}

fn infer_type_solution(model: &ProgramModel) -> TypeSolution {
    let mut solution = TypeSolution::new();
    for module in model.modules() {
        let Some(slice) = model.input().module_source_slice(module.id) else {
            continue;
        };
        let path = std::path::Path::new(slice.source_file_path);
        let Ok(annotations) = collect_top_level_literal_type_annotations(
            slice.source,
            Some(path),
            reverts_js::ParseGoal::TypeScript,
        ) else {
            continue;
        };
        for annotation in annotations {
            solution.insert(
                module.id,
                annotation.binding,
                inferred_type_from_generated(annotation.kind),
            );
        }
    }
    solution
}

const fn inferred_type_from_generated(kind: GeneratedTypeKind) -> InferredType {
    match kind {
        GeneratedTypeKind::Unknown => InferredType::Unknown,
        GeneratedTypeKind::Never => InferredType::Never,
        GeneratedTypeKind::String => InferredType::String,
        GeneratedTypeKind::Number => InferredType::Number,
        GeneratedTypeKind::Boolean => InferredType::Boolean,
        GeneratedTypeKind::BigInt => InferredType::BigInt,
        GeneratedTypeKind::Null => InferredType::Null,
        GeneratedTypeKind::Undefined => InferredType::Undefined,
    }
}

const ENRICHMENT_FUNCTION_FINGERPRINT_MODULE_LIMIT: usize = 1024;

fn should_collect_enrichment_function_fingerprints(model: &ProgramModel) -> bool {
    model.modules().len() <= ENRICHMENT_FUNCTION_FINGERPRINT_MODULE_LIMIT
        || std::env::var_os("REVERTS_COLLECT_FUNCTION_FINGERPRINTS").is_some()
}

fn audit_ast_fact_extraction(model: &ProgramModel) -> AuditReport {
    let mut audit = AuditReport::default();
    // Parser failures on a single bundle source mean we couldn't extract
    // AST facts for that module. Per ADR 0002 we surface the failure as a
    // warning so the rest of the project can emit; the planner sees the
    // affected module as having no definitions/imports/exports and the
    // emitter produces only what it can back with facts. The audit names
    // the failing module so the consumer knows where the gap is.
    for error in model.graph().ast_errors() {
        audit.push(
            AuditFinding::warning(FindingCode::AstFactExtractionFailed, error.message.clone())
                .with_module(error.module_id.0.to_string())
                .with_binding(error.path.clone()),
        );
    }
    audit
}

/// Parse the parent source file id from a synthetic source path of the form
/// `__reverts_synthetic__/<parent_id>/<name>.js` (produced by reverts-bundle
/// for reconstructed esbuild multi-handle modules). Mirrors the planner-side
/// helper since reverts-analyze can't depend on reverts-planner.
fn synthetic_parent_source_file_id(path: &str) -> Option<u32> {
    path.strip_prefix("__reverts_synthetic__/")?
        .split_once('/')
        .and_then(|(parent, _rest)| parent.parse::<u32>().ok())
}

/// True when `binding` is a runtime-helper binding in this module's source
/// file's prelude — or, for reconstructed synthetic modules, in the PARENT
/// source file's prelude. Such bindings (e.g. esbuild `__commonJS` aliases
/// `We`/`St` in source 6's prelude) are not real missing-definitions: the
/// planner's helper-rename pass lowers `helper(...)` calls to `lazyModule`/
/// `lazyValue` and the raw alias is gone in the emitted output.
fn is_runtime_helper_binding(
    model: &ProgramModel,
    module_id: ModuleId,
    binding: &BindingName,
) -> bool {
    let Some(module) = model.input().modules.iter().find(|m| m.id == module_id) else {
        return false;
    };
    let Some(source_file_id) = module.source_file_id else {
        return false;
    };
    let parent_id = model
        .input()
        .source_files
        .iter()
        .find(|sf| sf.id == source_file_id)
        .and_then(|sf| synthetic_parent_source_file_id(sf.path.as_str()));
    let mut candidates = vec![source_file_id];
    if let Some(parent) = parent_id {
        candidates.push(parent);
    }
    candidates.iter().any(|id| {
        model
            .graph()
            .runtime_prelude(*id)
            .map(|prelude| prelude.bindings.contains_key(binding))
            .unwrap_or(false)
    })
}

fn audit_def_use_graph(model: &ProgramModel) -> AuditReport {
    let mut audit = AuditReport::default();
    // A bare `MissingDefinition` reflects an incomplete bundle slice: the
    // referenced binding lives outside our extraction. Per ADR 0002 we
    // surface the missing read/write as a warning and let emission proceed
    // — the TypeScript output will reference the unresolved name and fail
    // type-check, but the user gets the faithful structure and audit pin-
    // points the missing binding for backfill.
    for (module_id, binding) in model.graph().def_use().unresolved_reads() {
        if is_ambient_binding(binding.as_str()) {
            continue;
        }
        if is_runtime_helper_binding(model, module_id, &binding) {
            continue;
        }
        audit.push(
            AuditFinding::warning(
                FindingCode::MissingDefinition,
                format!("binding '{binding}' is read without a local definition or import"),
            )
            .with_module(module_id.0.to_string())
            .with_binding(binding.as_str()),
        );
    }
    for (module_id, binding) in model.graph().def_use().unresolved_writes() {
        if is_ambient_binding(binding.as_str()) {
            continue;
        }
        if is_runtime_helper_binding(model, module_id, &binding) {
            continue;
        }
        audit.push(
            AuditFinding::warning(
                FindingCode::MissingDefinition,
                format!("binding '{binding}' is written without a local definition or import"),
            )
            .with_module(module_id.0.to_string())
            .with_binding(binding.as_str()),
        );
    }
    audit
}

// NOTE: a per-module `DuplicateTopLevelBinding` audit was removed here. It
// counted raw top-level Definition facts, but for validly-parsed input a
// repeated top-level binding can only be `var`/`function` hoisting (duplicate
// `let`/`const` is a parse error), which the def-use graph already dedupes to
// a single binding and which emission preserves as valid JS (DeclaratorSplit
// keeps the declaration kind). It therefore only ever produced false
// positives. A genuine duplicate hazard — distinct module owners colliding on
// one *output file* — is a per-output-file concern for the planner, not this
// per-module analysis pass.

/// Synthesize `Module` dependency edges for free reads that resolve to a
/// single defining module. For each unresolved read `(M, B)` where exactly one
/// other module `O` defines `B`, emit an edge `M -> O`; the planner's
/// `source_module_wiring` then imports `B` from `O`. Ambiguous reads (a name
/// defined by several modules — the minified-collision shape) and ambient
/// globals are skipped: we never guess an owner. Edges already present in the
/// input are not duplicated; one edge per `(from, to)` pair covers all shared
/// bindings since the planner intersects reads with the owner's exports.
struct SynthesizedModuleWiring {
    edges: Vec<ModuleDependencyInput>,
    imports: Vec<(ModuleId, BindingName)>,
}

fn synthesize_module_dependency_edges(model: &ProgramModel) -> SynthesizedModuleWiring {
    let def_use = model.graph().def_use();
    let existing: BTreeSet<(ModuleId, ModuleId)> = model
        .input()
        .dependencies
        .iter()
        .filter_map(|dependency| match dependency.target {
            ModuleDependencyTarget::Module(target) => Some((dependency.from_module_id, target)),
            ModuleDependencyTarget::Package { .. } => None,
        })
        .collect();

    // Each non-ambient free read paired with its candidate owner modules.
    let mut pending_by_read: BTreeMap<(ModuleId, BindingName), BTreeSet<ModuleId>> =
        BTreeMap::new();
    for (module_id, binding) in def_use.unresolved_reads() {
        if is_ambient_binding(binding.as_str()) {
            continue;
        }
        let owners: BTreeSet<ModuleId> = def_use
            .modules_defining(&binding)
            .into_iter()
            .filter(|owner| *owner != module_id)
            .collect();
        if !owners.is_empty() {
            pending_by_read
                .entry((module_id, binding))
                .or_default()
                .extend(owners);
        }
    }
    // Some bundler factory calls live inside nested closures. The def-use
    // graph intentionally scopes those reads, so a sibling factory call like
    // `var d = dep()` may not appear in `unresolved_reads()`. For module
    // dependency synthesis, scan source identifier reads as a conservative
    // backstop and still require a single owner before adding an edge.
    for module in model.modules() {
        let Some(slice) = model.input().module_source_slice(module.id) else {
            continue;
        };
        let Ok(facts) = collect_identifier_read_facts(
            slice.source,
            Some(std::path::Path::new(slice.source_file_path)),
            ParseGoal::TypeScript,
        ) else {
            continue;
        };
        for fact in facts {
            let binding = BindingName::new(fact.name);
            if is_ambient_binding(binding.as_str())
                || def_use.has_definition_or_import(module.id, &binding)
            {
                continue;
            }
            let owners: BTreeSet<ModuleId> = def_use
                .modules_defining(&binding)
                .into_iter()
                .filter(|owner| *owner != module.id)
                .collect();
            if !owners.is_empty() {
                pending_by_read
                    .entry((module.id, binding))
                    .or_default()
                    .extend(owners);
            }
        }
    }
    let pending: Vec<(ModuleId, BindingName, BTreeSet<ModuleId>)> = pending_by_read
        .into_iter()
        .map(|((module_id, binding), owners)| (module_id, binding, owners))
        .collect();

    // Resolve to a single owner, growing the dependency graph to a fixed point.
    // A read with one candidate resolves directly; an ambiguous read (a name
    // defined by several modules — the minified-collision shape) resolves only
    // when exactly one candidate is reachable from the reader via the
    // dependency edges already proven. Edges added this way feed the next
    // round's reachability. We never pick among equally-reachable candidates.
    let mut edges: BTreeSet<(ModuleId, ModuleId)> = existing.clone();
    let mut imports: Vec<(ModuleId, BindingName)> = Vec::new();
    let mut resolved = vec![false; pending.len()];
    loop {
        let reachable = reachable_modules(&edges);
        let empty = BTreeSet::new();
        let mut added = false;
        for (index, (module_id, binding, owners)) in pending.iter().enumerate() {
            if resolved[index] {
                continue;
            }
            let chosen = if owners.len() == 1 {
                owners.iter().next().copied()
            } else {
                let reach = reachable.get(module_id).unwrap_or(&empty);
                let mut reachable_owners = owners.iter().filter(|owner| reach.contains(owner));
                match (reachable_owners.next(), reachable_owners.next()) {
                    (Some(owner), None) => Some(*owner),
                    _ => None,
                }
            };
            if let Some(owner) = chosen {
                imports.push((*module_id, binding.clone()));
                if edges.insert((*module_id, owner)) {
                    added = true;
                }
                resolved[index] = true;
            }
        }
        if !added {
            break;
        }
    }

    let edges = edges
        .difference(&existing)
        .map(|(from_module_id, target)| ModuleDependencyInput {
            from_module_id: *from_module_id,
            target: ModuleDependencyTarget::Module(*target),
        })
        .collect();
    SynthesizedModuleWiring { edges, imports }
}

/// Transitive reachability per source module over the module-dependency edges.
fn reachable_modules(
    edges: &BTreeSet<(ModuleId, ModuleId)>,
) -> BTreeMap<ModuleId, BTreeSet<ModuleId>> {
    let mut adjacency: BTreeMap<ModuleId, Vec<ModuleId>> = BTreeMap::new();
    for (from, to) in edges {
        adjacency.entry(*from).or_default().push(*to);
    }
    let mut out: BTreeMap<ModuleId, BTreeSet<ModuleId>> = BTreeMap::new();
    for source in adjacency.keys().copied() {
        let mut seen = BTreeSet::new();
        let mut stack = vec![source];
        while let Some(node) = stack.pop() {
            if let Some(targets) = adjacency.get(&node) {
                for target in targets {
                    if seen.insert(*target) {
                        stack.push(*target);
                    }
                }
            }
        }
        out.insert(source, seen);
    }
    out
}

fn is_ambient_binding(binding: &str) -> bool {
    // Three families:
    //   1. ECMAScript built-in objects and well-known globals (ECMA-262)
    //   2. Web/DOM platform globals defined by W3C/WHATWG specs and present
    //      on `Window` in every modern browser
    //   3. Node/Bun/Deno runtime globals (Buffer, process, require, …)
    // Anything here must be guaranteed-resolvable by the JavaScript runtime
    // without a local declaration or import. Module/CJS sentinels (exports,
    // module, require, __dirname, __filename) are scoped, but the planner
    // treats them as ambient since they're injected by the host loader.
    matches!(
        binding,
        // --- ECMAScript built-ins (constructors + namespaces) ---
        "AggregateError"
            | "Array"
            | "ArrayBuffer"
            | "AsyncFunction"
            | "AsyncGenerator"
            | "AsyncGeneratorFunction"
            | "AsyncIterator"
            | "Atomics"
            | "BigInt"
            | "BigInt64Array"
            | "BigUint64Array"
            | "Boolean"
            | "DataView"
            | "Date"
            | "Error"
            | "EvalError"
            | "FinalizationRegistry"
            | "Float16Array"
            | "Float32Array"
            | "Float64Array"
            | "Function"
            | "Generator"
            | "GeneratorFunction"
            | "Infinity"
            | "Int8Array"
            | "Int16Array"
            | "Int32Array"
            | "Intl"
            | "Iterator"
            | "JSON"
            | "Map"
            | "Math"
            | "NaN"
            | "Number"
            | "Object"
            | "Promise"
            | "Proxy"
            | "RangeError"
            | "ReferenceError"
            | "Reflect"
            | "RegExp"
            | "Set"
            | "SharedArrayBuffer"
            | "String"
            | "Symbol"
            | "SyntaxError"
            | "TypeError"
            | "URIError"
            | "Uint8Array"
            | "Uint8ClampedArray"
            | "Uint16Array"
            | "Uint32Array"
            | "WeakMap"
            | "WeakRef"
            | "WeakSet"
            | "decodeURI"
            | "decodeURIComponent"
            | "encodeURI"
            | "encodeURIComponent"
            | "escape"
            | "eval"
            | "globalThis"
            | "isFinite"
            | "isNaN"
            | "parseFloat"
            | "parseInt"
            | "undefined"
            | "unescape"
            // --- Web / DOM platform globals (HTML, DOM, Fetch, Streams, IDB, …) ---
            | "AbortController"
            | "AbortSignal"
            | "Animation"
            | "AnimationEvent"
            | "AudioContext"
            | "AudioBuffer"
            | "BarcodeDetector"
            | "BeforeUnloadEvent"
            | "Blob"
            | "BroadcastChannel"
            | "Cache"
            | "CacheStorage"
            | "Comment"
            | "CompressionStream"
            | "CountQueuingStrategy"
            | "CustomElementRegistry"
            | "CustomEvent"
            | "DOMException"
            | "DOMMatrix"
            | "DOMParser"
            | "DOMPoint"
            | "DOMRect"
            | "DOMRectReadOnly"
            | "DOMStringMap"
            | "DOMTokenList"
            | "DecompressionStream"
            | "DataTransfer"
            | "DataTransferItem"
            | "DataTransferItemList"
            | "DragEvent"
            | "Document"
            | "DocumentFragment"
            | "Element"
            | "ErrorEvent"
            | "Event"
            | "EventSource"
            | "EventTarget"
            | "File"
            | "FileList"
            | "FileReader"
            | "FocusEvent"
            | "FontFace"
            | "FontFaceSet"
            | "FormData"
            | "FormDataEvent"
            | "Gamepad"
            | "GamepadEvent"
            | "Geolocation"
            | "GeolocationCoordinates"
            | "GeolocationPosition"
            | "HTMLAnchorElement"
            | "HTMLAreaElement"
            | "HTMLAudioElement"
            | "HTMLBRElement"
            | "HTMLBaseElement"
            | "HTMLBodyElement"
            | "HTMLButtonElement"
            | "HTMLCanvasElement"
            | "HTMLCollection"
            | "HTMLDListElement"
            | "HTMLDataElement"
            | "HTMLDataListElement"
            | "HTMLDetailsElement"
            | "HTMLDialogElement"
            | "HTMLDivElement"
            | "HTMLDocument"
            | "HTMLElement"
            | "HTMLEmbedElement"
            | "HTMLFieldSetElement"
            | "HTMLFontElement"
            | "HTMLFormElement"
            | "HTMLFrameElement"
            | "HTMLFrameSetElement"
            | "HTMLHRElement"
            | "HTMLHeadElement"
            | "HTMLHeadingElement"
            | "HTMLHtmlElement"
            | "HTMLIFrameElement"
            | "HTMLImageElement"
            | "HTMLInputElement"
            | "HTMLLIElement"
            | "HTMLLabelElement"
            | "HTMLLegendElement"
            | "HTMLLinkElement"
            | "HTMLMapElement"
            | "HTMLMediaElement"
            | "HTMLMenuElement"
            | "HTMLMetaElement"
            | "HTMLMeterElement"
            | "HTMLModElement"
            | "HTMLOListElement"
            | "HTMLObjectElement"
            | "HTMLOptGroupElement"
            | "HTMLOptionElement"
            | "HTMLOutputElement"
            | "HTMLParagraphElement"
            | "HTMLParamElement"
            | "HTMLPictureElement"
            | "HTMLPreElement"
            | "HTMLProgressElement"
            | "HTMLQuoteElement"
            | "HTMLScriptElement"
            | "HTMLSelectElement"
            | "HTMLSlotElement"
            | "HTMLSourceElement"
            | "HTMLSpanElement"
            | "HTMLStyleElement"
            | "HTMLTableCaptionElement"
            | "HTMLTableCellElement"
            | "HTMLTableColElement"
            | "HTMLTableElement"
            | "HTMLTableRowElement"
            | "HTMLTableSectionElement"
            | "HTMLTemplateElement"
            | "HTMLTextAreaElement"
            | "HTMLTimeElement"
            | "HTMLTitleElement"
            | "HTMLTrackElement"
            | "HTMLUListElement"
            | "HTMLUnknownElement"
            | "HTMLVideoElement"
            | "HashChangeEvent"
            | "Headers"
            | "History"
            | "IDBCursor"
            | "IDBCursorWithValue"
            | "IDBDatabase"
            | "IDBFactory"
            | "IDBIndex"
            | "IDBKeyRange"
            | "IDBObjectStore"
            | "IDBOpenDBRequest"
            | "IDBRequest"
            | "IDBTransaction"
            | "IDBVersionChangeEvent"
            | "Image"
            | "ImageBitmap"
            | "ImageDecoder"
            | "ImageData"
            | "InputEvent"
            | "IntersectionObserver"
            | "IntersectionObserverEntry"
            | "KeyboardEvent"
            | "Location"
            | "MediaQueryList"
            | "MediaQueryListEvent"
            | "MediaRecorder"
            | "MediaSource"
            | "MediaStream"
            | "MediaStreamTrack"
            | "MessageChannel"
            | "MessageEvent"
            | "MessagePort"
            | "MimeType"
            | "MimeTypeArray"
            | "MouseEvent"
            | "MutationObserver"
            | "MutationRecord"
            | "NamedNodeMap"
            | "Navigator"
            | "Node"
            | "NodeFilter"
            | "NodeIterator"
            | "NodeList"
            | "Notification"
            | "OffscreenCanvas"
            | "OffscreenCanvasRenderingContext2D"
            | "PageTransitionEvent"
            | "Path2D"
            | "Performance"
            | "PerformanceEntry"
            | "PerformanceMark"
            | "PerformanceMeasure"
            | "PerformanceObserver"
            | "PerformanceObserverEntryList"
            | "PerformanceResourceTiming"
            | "PointerEvent"
            | "PopStateEvent"
            | "ProgressEvent"
            | "Range"
            | "ReadableByteStreamController"
            | "ReadableStream"
            | "ReadableStreamBYOBReader"
            | "ReadableStreamBYOBRequest"
            | "ReadableStreamDefaultController"
            | "ReadableStreamDefaultReader"
            | "Request"
            | "ResizeObserver"
            | "ResizeObserverEntry"
            | "Response"
            | "Screen"
            | "ScreenOrientation"
            | "SecurityPolicyViolationEvent"
            | "Selection"
            | "ServiceWorker"
            | "ServiceWorkerContainer"
            | "ServiceWorkerRegistration"
            | "ShadowRoot"
            | "SharedWorker"
            | "Storage"
            | "StorageEvent"
            | "StyleSheet"
            | "StyleSheetList"
            | "SubmitEvent"
            | "Text"
            | "TextDecoder"
            | "TextDecoderStream"
            | "TextEncoder"
            | "TextEncoderStream"
            | "TextMetrics"
            | "TextTrack"
            | "TextTrackCue"
            | "TextTrackList"
            | "TimeRanges"
            | "Touch"
            | "TouchEvent"
            | "TouchList"
            | "TrackEvent"
            | "TransformStream"
            | "TransformStreamDefaultController"
            | "TransitionEvent"
            | "TreeWalker"
            | "UIEvent"
            | "URL"
            | "URLSearchParams"
            | "ValidityState"
            | "VideoTrack"
            | "VideoTrackList"
            | "VTTCue"
            | "VisualViewport"
            | "WebGL2RenderingContext"
            | "WebGLBuffer"
            | "WebGLContextEvent"
            | "WebGLFramebuffer"
            | "WebGLProgram"
            | "WebGLQuery"
            | "WebGLRenderbuffer"
            | "WebGLRenderingContext"
            | "WebGLShader"
            | "WebGLTexture"
            | "WebGLTransformFeedback"
            | "WebGLUniformLocation"
            | "WebGLVertexArrayObject"
            | "WebSocket"
            | "WheelEvent"
            | "Window"
            | "Worker"
            | "Worklet"
            | "WritableStream"
            | "WritableStreamDefaultController"
            | "WritableStreamDefaultWriter"
            | "XMLDocument"
            | "XMLHttpRequest"
            | "XMLHttpRequestEventTarget"
            | "XMLHttpRequestUpload"
            | "XMLSerializer"
            | "XPathEvaluator"
            | "XPathExpression"
            | "XPathResult"
            | "XSLTProcessor"
            | "alert"
            | "atob"
            | "btoa"
            | "cancelAnimationFrame"
            | "cancelIdleCallback"
            | "caches"
            | "clearImmediate"
            | "clearInterval"
            | "clearTimeout"
            | "close"
            | "closed"
            | "confirm"
            | "console"
            | "createImageBitmap"
            | "crypto"
            | "customElements"
            | "document"
            | "fetch"
            | "frames"
            | "getComputedStyle"
            | "getSelection"
            | "history"
            | "indexedDB"
            | "innerHeight"
            | "innerWidth"
            | "isSecureContext"
            | "localStorage"
            | "location"
            | "matchMedia"
            | "navigator"
            | "onbeforeunload"
            | "onerror"
            | "onhashchange"
            | "onload"
            | "onmessage"
            | "onpopstate"
            | "onstorage"
            | "open"
            | "opener"
            | "origin"
            | "outerHeight"
            | "outerWidth"
            | "parent"
            | "performance"
            | "postMessage"
            | "print"
            | "prompt"
            | "queueMicrotask"
            | "reportError"
            | "requestAnimationFrame"
            | "requestIdleCallback"
            | "screen"
            | "scroll"
            | "scrollBy"
            | "scrollTo"
            | "scrollX"
            | "scrollY"
            | "self"
            | "sessionStorage"
            | "setImmediate"
            | "setInterval"
            | "setTimeout"
            | "showOpenFilePicker"
            | "showSaveFilePicker"
            | "showDirectoryPicker"
            | "stop"
            | "structuredClone"
            | "top"
            | "visualViewport"
            | "window"
            // --- Node / Bun / Deno runtime globals ---
            | "Buffer"
            | "Bun"
            | "Deno"
            | "__dirname"
            | "__filename"
            | "browser"
            | "chrome"
            | "exports"
            | "global"
            | "module"
            | "process"
            | "require"
    )
}

/// Walk the lightweight CFG and flag statements that follow a top-level
/// `return` or `throw` via a Sequential edge — those are unreachable. Skips
/// the implicit Exit node so the final terminator does not falsely fire.
fn audit_unreachable_top_level_code(model: &ProgramModel) -> AuditReport {
    let mut audit = AuditReport::default();
    let cfg = model.graph().control_flow();
    for module in model.modules() {
        let nodes = cfg.nodes_for(module.id);
        if nodes.is_empty() {
            continue;
        }
        for edge in cfg.edges_for(module.id) {
            if edge.kind != ControlFlowEdgeKind::Sequential {
                continue;
            }
            let Some(from_node) = nodes.iter().find(|node| node.id == edge.from) else {
                continue;
            };
            if !matches!(
                from_node.kind,
                ControlFlowNodeKind::Return | ControlFlowNodeKind::Throw
            ) {
                continue;
            }
            let Some(to_node) = nodes.iter().find(|node| node.id == edge.to) else {
                continue;
            };
            if to_node.kind == ControlFlowNodeKind::Exit {
                continue;
            }
            audit.push(
                AuditFinding::error(
                    FindingCode::UnreachableTopLevelCode,
                    "module body contains a statement that follows a top-level return or throw",
                )
                .with_module(module.id.0.to_string()),
            );
        }
    }
    audit
}

/// Flag statements unreachable within their enclosing function, using the oxc
/// intraprocedural CFG projected onto [`IntraproceduralFlow`]. Complements
/// `audit_unreachable_top_level_code` (which only sees the module top level) by
/// reaching inside function bodies and nested branches. Per ADR 0002 this is a
/// warning: dead code in the input is surfaced, not repaired.
fn audit_unreachable_function_code(model: &ProgramModel) -> AuditReport {
    let mut audit = AuditReport::default();
    let flow = model.graph().intraprocedural_flow();
    for module in model.modules() {
        for span in flow.unreachable_in(module.id) {
            audit.push(
                AuditFinding::warning(
                    FindingCode::UnreachableFunctionCode,
                    format!(
                        "function body contains unreachable code at bytes {}..{}",
                        span.start, span.end
                    ),
                )
                .with_module(module.id.0.to_string()),
            );
        }
    }
    audit
}

fn audit_binding_shape_conflicts(binding_shapes: &BindingShapeSolution) -> AuditReport {
    let mut audit = AuditReport::default();
    for conflict in binding_shapes.conflicts() {
        let code = binding_shape_conflict_code(conflict.existing_kind, conflict.incoming_kind);
        let message = format!(
            "binding has incompatible shape constraints: {:?} requires {:?}, {:?} requires {:?}",
            conflict.existing_kind,
            conflict.existing_shape,
            conflict.incoming_kind,
            conflict.incoming_shape
        );
        // Shape conflicts describe the recovered input's inconsistent use of
        // a binding (common in minified/bundle slices). Per ADR 0002 the
        // decompiler must surface the risk but still emit the faithful source
        // instead of stranding the whole project.
        let finding = AuditFinding::warning(code, message);
        audit.push(
            finding
                .with_module(conflict.module_id.0.to_string())
                .with_binding(conflict.binding.as_str().to_string()),
        );
    }
    audit
}

/// Surface the input-bundle pattern `X = (await fetch(...)).data.value;`
/// followed by an unguarded member-read on a binding that aliases `X`.
/// ADR 0002 forbids repairing the input — we warn so the user knows the
/// original bundle has a latent null deref, while keeping the emit
/// faithful.
///
/// The alias closure resolves direct accesses (`X.foo`) and any
/// module-scope alias chain (`A = X; A.foo`, `A = getX(); A.foo` where
/// `getX()` returns X and the call result is bound at module scope).
/// Indirect access via function-local aliases (e.g. `let A` inside a
/// function) is currently NOT caught — locals are filtered out of the
/// fact extractor to avoid name collisions across nested functions, and
/// proper scope-qualified binding identity is a follow-up.
fn audit_unprotected_nullable_member_reads(model: &ProgramModel) -> AuditReport {
    use std::collections::BTreeSet;
    let mut audit = AuditReport::default();
    let def_use = model.graph().def_use();
    let maybe_nullable = def_use.maybe_nullable_writes();
    if maybe_nullable.is_empty() {
        return audit;
    }
    let mut read_targets: BTreeSet<(reverts_ir::ModuleId, reverts_ir::BindingName)> =
        BTreeSet::new();
    for constraint in def_use.constraints() {
        if matches!(
            constraint.kind,
            BindingConstraintKind::MemberRead | BindingConstraintKind::MemberWrite
        ) {
            read_targets.insert((constraint.module_id, constraint.binding.clone()));
        }
    }
    // Build the reverse map: for each maybe-nullable source binding, find
    // every binding that aliases (directly or transitively) back to it.
    // The audit then fires when any of those aliases is member-read.
    let mut already_reported: BTreeSet<(reverts_ir::ModuleId, reverts_ir::BindingName)> =
        BTreeSet::new();
    for (module_id, read_target) in &read_targets {
        let aliases = def_use.alias_sources_of(*module_id, read_target.as_str());
        for alias in &aliases {
            let key = (*module_id, alias.clone());
            if !maybe_nullable.contains(&key) || already_reported.contains(&key) {
                continue;
            }
            already_reported.insert(key);
            audit.push(
                // Documented as a warning per ADR 0002: the decompiler is
                // faithful, not corrective. Crash potential exists in the
                // original bundle; we surface it but don't strand emission.
                AuditFinding::warning(
                    FindingCode::UnprotectedNullableMemberRead,
                    format!(
                        "binding '{}' is assigned from a member chain on a call/await result and later member-read without a null guard — the original bundle can crash here",
                        alias.as_str()
                    ),
                )
                .with_module(module_id.0.to_string())
                .with_binding(alias.as_str().to_string()),
            );
        }
    }
    audit
}

fn binding_shape_conflict_code(
    existing_kind: BindingConstraintKind,
    incoming_kind: BindingConstraintKind,
) -> FindingCode {
    if matches!(
        (existing_kind, incoming_kind),
        (
            BindingConstraintKind::ObjectLiteralDeclaration,
            BindingConstraintKind::Call
        ) | (
            BindingConstraintKind::Call,
            BindingConstraintKind::ObjectLiteralDeclaration
        )
    ) {
        FindingCode::CallableEmittedAsNonCallable
    } else {
        FindingCode::AmbiguousBindingShape
    }
}

/// Recover a real layout path from a `modules/<id>-<slug>.ts` namespaced path by
/// stripping the `modules/<id>-` prefix — but only when what remains is a
/// multi-segment directory path. A bare `modules/<id>-Ov6.ts` minified name yields
/// `Ov6.ts`, which has no directory and must NOT be hoisted to the project root.
fn clean_module_layout_path(module_id: ModuleId, semantic_path: &str) -> Option<String> {
    let prefix = format!("modules/{}-", module_id.0);
    let remainder = semantic_path.strip_prefix(prefix.as_str())?;
    if remainder.contains('/') && is_safe_ts_module_path(remainder) {
        Some(remainder.to_string())
    } else {
        None
    }
}

/// A POSIX-relative `.ts`/`.tsx` path whose every segment is a safe filename
/// (alphanumeric / `_` / `-` / `.`, never empty or `.`/`..`).
fn is_safe_ts_module_path(path: &str) -> bool {
    if !path.ends_with(".ts") && !path.ends_with(".tsx") {
        return false;
    }
    path.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    })
}

fn assign_semantic_names(model: &ProgramModel) -> SemanticNameMap {
    let mut semantic_names = SemanticNameMap::default();
    let mut used_by_module: BTreeMap<ModuleId, BTreeSet<String>> = BTreeMap::new();
    let mut mapped_originals = BTreeSet::<(ModuleId, String)>::new();

    // Module output paths arrive namespaced as `modules/<id>-<slug>.ts` to
    // guarantee uniqueness, which buries an otherwise-meaningful agent/reference
    // layout path (e.g. `modules/228340-smithy/http-request-handler.ts`). Recover
    // the clean layout path by dropping the `modules/<id>-` prefix when (a) what
    // remains is a real multi-segment directory path and (b) it is globally unique.
    // Uniqueness is the only thing the prefix guaranteed, so dropping it where
    // uniqueness already holds restores the intended layout WITHOUT risking a path
    // collision (which would drop a module). Bare/minified names and any collision
    // keep the namespaced form. This map is the single source BOTH the planner
    // (emission + cross-file import rewrites) and the pipeline (symbol index) read,
    // so a module rename / path adjustment stays consistent end to end.
    // Modules that own co-located assets must NOT be moved: an asset's emitted
    // `output_path` is a fixed value (`modules/<id>-<dir>/<asset>`) that encodes the
    // module's namespaced directory, and the module's `require('./asset')` is
    // relative to that directory. Cleaning the module path without re-rooting the
    // asset would dangle the asset reference. Detect asset owners by the shared
    // `modules/<id>-` prefix and keep them namespaced.
    let asset_owner_modules: BTreeSet<u32> = model
        .input()
        .assets
        .iter()
        .filter_map(|asset| asset.output_path.strip_prefix("modules/"))
        .filter_map(|rest| rest.split_once('-'))
        .filter_map(|(id, _)| id.parse::<u32>().ok())
        .collect();

    let mut clean_layout_paths: BTreeMap<ModuleId, String> = BTreeMap::new();
    let mut clean_path_counts: BTreeMap<String, usize> = BTreeMap::new();
    for module in model.modules() {
        if asset_owner_modules.contains(&module.id.0) {
            continue;
        }
        if let Some(clean) = clean_module_layout_path(module.id, module.semantic_path.as_str()) {
            clean_layout_paths.insert(module.id, clean.clone());
            *clean_path_counts.entry(clean).or_default() += 1;
        }
    }
    for module in model.modules() {
        let path = clean_layout_paths
            .get(&module.id)
            .filter(|clean| clean_path_counts.get(*clean) == Some(&1))
            .cloned()
            .unwrap_or_else(|| module.semantic_path.clone());
        semantic_names.insert_module_path(module.id, path);
    }

    for symbol in model.symbols() {
        if symbol.scope != SymbolScope::Module {
            continue;
        }
        if !mapped_originals.insert((symbol.module_id, symbol.name.clone())) {
            continue;
        }
        let Some(semantic) = symbol.semantic_name.as_deref() else {
            continue;
        };
        if is_generated_placeholder_identifier(semantic)
            || sanitize_identifier(semantic) != semantic
        {
            continue;
        }
        if !used_by_module
            .entry(symbol.module_id)
            .or_default()
            .insert(semantic.to_string())
        {
            continue;
        }
        semantic_names.insert_binding(symbol.module_id, symbol.name.clone(), semantic);
    }

    semantic_names
}

fn detect_compiler_profile(model: &ProgramModel) -> CompilerProfile {
    let mut identifiers_by_module = BTreeMap::<ModuleId, BTreeSet<String>>::new();
    let mut wrappers_by_module = BTreeMap::<ModuleId, BTreeSet<AstWrapperKind>>::new();
    for fact in model.graph().ast_facts() {
        if let Some(binding) = &fact.binding {
            identifiers_by_module
                .entry(fact.module_id)
                .or_default()
                .insert(binding.as_str().to_string());
        }
        if let AstFactKind::WrapperRegion(kind) = fact.kind {
            wrappers_by_module
                .entry(fact.module_id)
                .or_default()
                .insert(kind);
        }
    }

    let mut profile = CompilerProfile::default();
    for module in model.modules() {
        let Some(source) = model.input().module_source_slice(module.id) else {
            continue;
        };
        let identifiers = identifiers_by_module
            .get(&module.id)
            .cloned()
            .unwrap_or_default();
        let wrappers = wrappers_by_module
            .get(&module.id)
            .cloned()
            .unwrap_or_default();
        profile.insert_module(
            module.id,
            detect_module_compiler_profile(source.source, &identifiers, &wrappers),
        );
    }
    profile
}

fn detect_module_compiler_profile(
    source: &str,
    identifiers: &BTreeSet<String>,
    wrappers: &BTreeSet<AstWrapperKind>,
) -> ModuleCompilerProfile {
    let mut evidence = Vec::new();
    let minified = looks_minified(source);
    if minified {
        evidence.push(CompilerEvidence::MinifiedLayout);
    }
    for wrapper in wrappers {
        evidence.push(CompilerEvidence::TopLevelIife(*wrapper));
    }

    let compiler = if collect_runtime_identifier_evidence(
        identifiers,
        WEBPACK_RUNTIME_IDENTIFIERS,
        &mut evidence,
    ) {
        CompilerKind::Webpack
    } else if collect_runtime_identifier_evidence(
        identifiers,
        ESBUILD_RUNTIME_IDENTIFIERS,
        &mut evidence,
    ) {
        CompilerKind::Esbuild
    } else if collect_runtime_identifier_evidence(
        identifiers,
        ROLLUP_RUNTIME_IDENTIFIERS,
        &mut evidence,
    ) {
        CompilerKind::Rollup
    } else if collect_runtime_identifier_evidence(
        identifiers,
        BABEL_RUNTIME_IDENTIFIERS,
        &mut evidence,
    ) {
        CompilerKind::Babel
    } else if minified {
        CompilerKind::Terser
    } else {
        CompilerKind::Unknown
    };

    ModuleCompilerProfile::new(compiler, minified, evidence)
}

/// Identifier-based detection. Runtime helper names are accepted only when the
/// AST fact extractor reports them as identifier evidence.
fn collect_runtime_identifier_evidence(
    identifiers: &BTreeSet<String>,
    candidates: &[&'static str],
    evidence: &mut Vec<CompilerEvidence>,
) -> bool {
    let mut matched = false;
    for candidate in candidates {
        if identifiers.contains(*candidate) {
            evidence.push(CompilerEvidence::Identifier((*candidate).to_string()));
            matched = true;
        }
    }
    matched
}

fn looks_minified(source: &str) -> bool {
    let byte_len = source.len();
    if byte_len < 120 {
        return false;
    }

    let non_empty_lines = source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if non_empty_lines.is_empty() {
        return false;
    }

    let max_line_len = non_empty_lines
        .iter()
        .map(|line| line.len())
        .max()
        .unwrap_or(0);
    let whitespace_count = source.bytes().filter(u8::is_ascii_whitespace).count();
    let whitespace_ratio = whitespace_count as f64 / byte_len as f64;
    let average_line_len = byte_len / non_empty_lines.len();

    (non_empty_lines.len() == 1 && max_line_len >= 120)
        || (average_line_len >= 160 && whitespace_ratio <= 0.12)
}

pub const WEBPACK_RUNTIME_IDENTIFIERS: &[&str] = &[
    "__webpack_require__",
    "__webpack_exports__",
    "__webpack_modules__",
    "__webpack_module_cache__",
    "webpackChunk",
    "webpackJsonp",
];

pub const ESBUILD_RUNTIME_IDENTIFIERS: &[&str] = &[
    "__defProp",
    "__export",
    "__copyProps",
    "__toESM",
    "__toCommonJS",
    "__commonJS",
    "__require",
];

pub const ROLLUP_RUNTIME_IDENTIFIERS: &[&str] = &[
    "commonjsGlobal",
    "getDefaultExportFromCjs",
    "getAugmentedNamespace",
    "_mergeNamespaces",
    "_interopNamespaceDefault",
    "_interopDefaultLegacy",
];

pub const BABEL_RUNTIME_IDENTIFIERS: &[&str] = &[
    "_interopRequireDefault",
    "_interopRequireWildcard",
    "_classCallCheck",
    "_createClass",
    "_defineProperty",
    "_inherits",
    "_possibleConstructorReturn",
    "regeneratorRuntime",
];

/// esbuild output wrapper function names. These are emitted by the esbuild
/// runtime around imported CJS modules, exported namespaces, and helper
/// inits; `reverts-js::normalize::BundlerWrapperUnwrapped` strips them for
/// `ast_hash` collision, `reverts-bundle::detectors::esbuild` recognises
/// them as module boundaries.
///
/// The definition lives in
/// [`reverts_js::normalize::bundler_wrapper_unwrapped::ESBUILD_WRAPPER_NAMES`]
/// and is re-exported here as the single stable public surface for callers
/// outside `reverts-js`.
pub use reverts_js::normalize::bundler_wrapper_unwrapped::ESBUILD_WRAPPER_NAMES;

fn resolve_package_imports(
    model: &ProgramModel,
    package_index: &PackageSurfaceIndex,
    audit: &mut AuditReport,
) -> Vec<PackageImportDecision> {
    let mut requested_imports = BTreeMap::<(ModuleId, String), bool>::new();
    for dependency in &model.input().dependencies {
        let ModuleDependencyTarget::Package { specifier } = &dependency.target else {
            continue;
        };
        requested_imports
            .entry((dependency.from_module_id, specifier.clone()))
            .or_insert(false);
    }

    for fact in model
        .graph()
        .ast_facts()
        .iter()
        .filter(|fact| fact.kind == AstFactKind::PackageImport)
    {
        let Some(specifier) = &fact.binding else {
            continue;
        };
        requested_imports.insert((fact.module_id, specifier.as_str().to_string()), true);
    }

    let mut decisions = Vec::new();

    for ((from_module_id, specifier), source_backed) in requested_imports {
        let resolution = package_index.resolve(&specifier);
        if let PackageResolution::Rejected { reason, .. } = &resolution {
            // The input bundle references a bare specifier whose package
            // surface isn't in the index (typically a package_source_cache
            // miss). Per ADR 0002 the decompiler is faithful, not
            // corrective: surface the unresolved specifier and let
            // emission proceed; the operator can backfill the cache.
            audit.push(
                AuditFinding::warning(FindingCode::UnresolvableBareImport, reason.clone())
                    .with_module(from_module_id.0.to_string())
                    .with_binding(specifier.clone()),
            );
        }

        decisions.push(PackageImportDecision::with_source_backed(
            from_module_id,
            BindingName::new(package_namespace_binding(&specifier)),
            resolution,
            source_backed,
        ));
    }

    decisions
}

fn package_namespace_binding(specifier: &str) -> String {
    let sanitized = sanitize_identifier(specifier);
    format!("__pkg_{sanitized}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use reverts_input::{
        InputBundle, InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
        PackageAttributionInput, PackageSurfaceInput, ProjectInput, SourceFileInput, SymbolInput,
    };
    use reverts_ir::ModuleId;
    use reverts_model::CompilerKind;
    use reverts_observe::FindingCode;
    use reverts_package::PackageResolution;

    use super::{ProgramModel, detect_module_compiler_profile, enrich_program};

    fn valid_rows() -> InputRows {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "app", "src/index.ts"));
        rows
    }

    #[test]
    fn aggregate_error_is_ambient() {
        assert!(super::is_ambient_binding("AggregateError"));
    }

    fn rows_with_application_source(source: &str) -> InputRows {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some(source.to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        rows
    }

    #[test]
    fn accepted_attribution_resolves_package_dependency() {
        let mut rows = valid_rows();
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "lodash/map".to_string(),
            },
        });
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.is_clean());
        assert!(matches!(
            output.program.package_imports()[0].resolution,
            PackageResolution::External { .. }
        ));
    }

    #[test]
    fn accepted_project_package_surface_resolves_source_backed_external_import() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("const client = require('undici'); export { client };".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        rows.package_surfaces
            .push(PackageSurfaceInput::accepted_external(
                "undici", "2.2.1", "undici",
            ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.is_clean());
        assert_eq!(output.program.package_imports().len(), 1);
        assert!(output.program.package_imports()[0].source_backed);
        assert!(matches!(
            output.program.package_imports()[0].resolution,
            PackageResolution::External { .. }
        ));
    }

    #[test]
    fn semantic_naming_does_not_generate_name_from_reserved_original() {
        let mut rows = valid_rows();
        rows.symbols.push(SymbolInput::new(ModuleId(1), "class"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));
        let binding = output
            .program
            .semantic_names()
            .binding_name(ModuleId(1), "class");

        assert!(binding.is_none());
    }

    #[test]
    fn semantic_name_hint_does_not_replace_source_binding_identity() {
        let mut rows = valid_rows();
        rows.symbols.push(
            SymbolInput::new(ModuleId(1), "$F1").with_semantic_name("lodashGlobalObjectInit"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));
        let binding = output
            .program
            .semantic_names()
            .binding_name(ModuleId(1), "$F1")
            .expect("semantic binding should be keyed by original identity");

        assert_eq!(binding.as_str(), "lodashGlobalObjectInit");
        assert!(
            output
                .program
                .semantic_names()
                .binding_name(ModuleId(1), "lodashGlobalObjectInit")
                .is_none()
        );
    }

    #[test]
    fn semantic_naming_ignores_generated_placeholder_hints() {
        let mut rows = valid_rows();
        rows.symbols.push(
            SymbolInput::new(ModuleId(1), "Rdr").with_semantic_name("module247SemanticSymbol001"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));
        let binding = output
            .program
            .semantic_names()
            .binding_name(ModuleId(1), "Rdr");

        assert!(binding.is_none());
    }

    #[test]
    fn semantic_naming_does_not_propagate_hints_to_unhinted_aliases() {
        let mut rows = rows_with_application_source("var a = 1; var b = a; console.log(a, b);");
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "a").with_semantic_name("settings"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));
        let names = output.program.semantic_names();

        assert_eq!(
            names
                .binding_name(ModuleId(1), "a")
                .expect("direct semantic name should exist")
                .as_str(),
            "settings"
        );
        assert!(names.binding_name(ModuleId(1), "b").is_none());
    }

    #[test]
    fn semantic_naming_ignores_export_names_without_explicit_hint() {
        let mut rows = valid_rows();
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "a").with_export_name("createClient"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(
            output
                .program
                .semantic_names()
                .binding_name(ModuleId(1), "a")
                .is_none()
        );
    }

    #[test]
    fn semantic_naming_ignores_invalid_explicit_hint_instead_of_sanitizing() {
        let mut rows = valid_rows();
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "a").with_semantic_name("create-client"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(
            output
                .program
                .semantic_names()
                .binding_name(ModuleId(1), "a")
                .is_none()
        );
    }

    #[test]
    fn semantic_naming_direct_alias_hint_wins_over_propagated_hint() {
        let mut rows = rows_with_application_source("var a = 1; var b = a; console.log(a, b);");
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "a").with_semantic_name("settings"));
        rows.symbols
            .push(SymbolInput::new(ModuleId(1), "b").with_semantic_name("preferredAlias"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));
        let names = output.program.semantic_names();

        assert_eq!(
            names
                .binding_name(ModuleId(1), "b")
                .expect("direct semantic name should exist")
                .as_str(),
            "preferredAlias"
        );
    }

    #[test]
    fn unknown_package_surface_reports_unresolvable_import() {
        let mut rows = valid_rows();
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "lodash/map".to_string(),
            },
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::UnresolvableBareImport));
        assert!(matches!(
            output.program.package_imports()[0].resolution,
            PackageResolution::Rejected { .. }
        ));
    }

    #[test]
    fn ast_bare_import_uses_package_surface_resolution_without_duplicate_dependency_rows() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("import { map } from 'lodash/map'; export const answer = map;".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.is_clean());
        assert_eq!(output.program.package_imports().len(), 1);
        assert!(output.program.package_imports()[0].source_backed);
    }

    #[test]
    fn ast_bare_import_without_surface_reports_unresolvable_import() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("import { map } from 'lodash/map'; export const answer = map;".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::UnresolvableBareImport));
        assert_eq!(output.program.package_imports().len(), 1);
        assert!(output.program.package_imports()[0].source_backed);
    }

    #[test]
    fn ast_bare_reexport_uses_package_surface_resolution() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("export { map as lodashMap } from 'lodash/map';".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.is_clean());
        assert_eq!(output.program.package_imports().len(), 1);
        assert!(output.program.package_imports()[0].source_backed);
        assert!(matches!(
            output.program.package_imports()[0].resolution,
            PackageResolution::External { .. }
        ));
    }

    #[test]
    fn ast_fact_extraction_failure_is_reported_as_audit_finding() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "broken.js",
            Some("const =".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::AstFactExtractionFailed));
    }

    #[test]
    fn compiler_profile_detector_classifies_runtime_fingerprints() {
        let no_wrappers = BTreeSet::new();
        assert_eq!(
            detect_module_compiler_profile("", &identifiers(["__webpack_require__"]), &no_wrappers)
                .compiler,
            CompilerKind::Webpack
        );
        assert_eq!(
            detect_module_compiler_profile("", &identifiers(["__toCommonJS"]), &no_wrappers)
                .compiler,
            CompilerKind::Esbuild
        );
        assert_eq!(
            detect_module_compiler_profile("", &identifiers(["commonjsGlobal"]), &no_wrappers)
                .compiler,
            CompilerKind::Rollup
        );
        assert_eq!(
            detect_module_compiler_profile(
                "",
                &identifiers(["_interopRequireDefault"]),
                &no_wrappers
            )
            .compiler,
            CompilerKind::Babel
        );
        assert_eq!(
            detect_module_compiler_profile(
                "function a(b){return b?b.c?b.c.d:b.c:b}var c={};for(var d=0;d<200;d++)c[d]=a({c:{d:d}});module.exports=c;function e(f){return f&&f.g?f.g.h:0}exports.e=e;",
                &BTreeSet::new(),
                &no_wrappers
            )
            .compiler,
            CompilerKind::Terser
        );
    }

    #[test]
    fn enrich_program_records_compiler_profile_from_ast_facts() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("__webpack_require__(1);".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert_eq!(
            output
                .program
                .compiler_profile()
                .module(ModuleId(1))
                .compiler,
            CompilerKind::Webpack
        );
    }

    #[test]
    fn top_level_iife_wrappers_are_recorded_as_compiler_evidence() {
        use reverts_graph::AstWrapperKind;
        use reverts_model::CompilerEvidence;

        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("(function(){ var x = 1; })();\n(()=>{ var y = 2; })();\n".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        let evidence = &output
            .program
            .compiler_profile()
            .module(ModuleId(1))
            .evidence;
        assert!(
            evidence.contains(&CompilerEvidence::TopLevelIife(
                AstWrapperKind::FunctionIife
            )),
            "expected FunctionIife evidence, got: {evidence:?}",
        );
        assert!(
            evidence.contains(&CompilerEvidence::TopLevelIife(AstWrapperKind::ArrowIife)),
            "expected ArrowIife evidence, got: {evidence:?}",
        );
    }

    #[test]
    fn top_level_arrow_iife_alone_does_not_classify_as_esbuild() {
        // A wrapper shape alone is not enough to infer the compiler. Without
        // identifier or explicit pattern evidence, classification stays Unknown.
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("(()=>{ var x = 1; x.foo = 2; })();\n".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert_eq!(
            output
                .program
                .compiler_profile()
                .module(ModuleId(1))
                .compiler,
            CompilerKind::Unknown,
        );
    }

    #[test]
    fn babel_es_module_marker_alone_stays_unknown() {
        // The `Object.defineProperty(exports, "__esModule", ...)` marker is a
        // common CJS artifact and is not enough evidence without a Babel
        // helper identifier extracted from the AST facts.
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(
                "\"use strict\";\nObject.defineProperty(exports, \"__esModule\", { value: true });\nexports.foo = 1;\n"
                    .to_string(),
            ),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert_eq!(
            output
                .program
                .compiler_profile()
                .module(ModuleId(1))
                .compiler,
            CompilerKind::Unknown,
        );
    }

    #[test]
    fn babel_jsx_runtime_import_alone_stays_unknown() {
        // JSX runtime import specifiers are ordinary dependency edges. They
        // do not prove Babel without a Babel helper identifier extracted from
        // the AST facts.
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(
                "import { jsx as _jsx } from \"react/jsx-runtime\";\nvar x = _jsx(\"div\", {});\n"
                    .to_string(),
            ),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert_eq!(
            output
                .program
                .compiler_profile()
                .module(ModuleId(1))
                .compiler,
            CompilerKind::Unknown,
        );
    }

    #[test]
    fn top_level_throw_followed_by_statement_is_reported_as_unreachable() {
        // CommonJS scripts may have a top-level `return` and ESM modules may
        // have a top-level `throw`; either way, code that follows them
        // unconditionally cannot run and the CFG-based audit must surface that.
        let source = "throw new Error(\"boom\");\nvar leftover = 1;\n";
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(source.to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(
            output.audit.has(FindingCode::UnreachableTopLevelCode),
            "expected UnreachableTopLevelCode finding, got: {:?}",
            output.audit.findings(),
        );
    }

    #[test]
    fn final_top_level_throw_alone_is_not_reported_as_unreachable() {
        // A single `throw` at the end of a module is reachable; the audit must
        // not fire on it.
        let source = "var x = 1;\nthrow new Error(\"boom\");\n";
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(source.to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(
            !output.audit.has(FindingCode::UnreachableTopLevelCode),
            "audit must not fire on a final throw, got: {:?}",
            output.audit.findings(),
        );
    }

    #[test]
    fn dead_code_inside_a_function_is_reported_as_unreachable_function_code() {
        // The dead statement is INSIDE a function body, so the top-level audit
        // cannot see it — only the intraprocedural CFG audit fires.
        let source = "function f() {\n  return 1;\n  globalThis.dead = 2;\n}\n";
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(source.to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(
            output.audit.has(FindingCode::UnreachableFunctionCode),
            "expected UnreachableFunctionCode finding, got: {:?}",
            output.audit.findings(),
        );
        // It is function-internal, so the top-level audit stays silent.
        assert!(
            !output.audit.has(FindingCode::UnreachableTopLevelCode),
            "top-level audit must not fire on function-internal dead code",
        );
    }

    #[test]
    fn straight_line_function_reports_no_unreachable_function_code() {
        let source = "function f(x) {\n  const y = x + 1;\n  return y;\n}\n";
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(source.to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(
            !output.audit.has(FindingCode::UnreachableFunctionCode),
            "no dead code expected, got: {:?}",
            output.audit.findings(),
        );
    }

    #[test]
    fn incompatible_ast_shape_facts_are_reported_as_audit_finding() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "enum-conflict.js",
            Some(
                r#"
var NativeModuleType;
(function (NativeModuleType) {
    NativeModuleType[NativeModuleType["File"] = 0] = "File";
})(NativeModuleType || (NativeModuleType = {}));
NativeModuleType();
"#
                .to_string(),
            ),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::AmbiguousBindingShape));
    }

    #[test]
    fn object_literal_called_as_function_reports_callable_shape_failure() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "callable-conflict.js",
            Some("const factory = {}; factory();".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::CallableEmittedAsNonCallable));
    }

    #[test]
    fn unresolved_ast_read_is_reported_as_missing_definition() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "missing-read.js",
            Some("missing();".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.has(FindingCode::MissingDefinition));
        assert!(output.audit.findings().iter().any(|finding| {
            finding.code == FindingCode::MissingDefinition
                && finding.binding.as_deref() == Some("missing")
        }));
    }

    #[test]
    fn runtime_helper_binding_read_is_not_reported_as_missing() {
        // A bundle's top-level __commonJS helper alias (`var St = (e,A) => () =>
        // (A||e((A={exports:{}}).exports,A),A.exports)`) sits OUTSIDE any module
        // span and becomes a prelude binding classified as `CommonJsWrapper`. A
        // module reading `St` (the planner will lower the `St(...)` call to
        // `lazyModule`) must NOT trip a `MissingDefinition` audit finding —
        // the binding is resolved at the prelude/lowering layer, not free.
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        let bundle_source = "var St=(e,A)=>()=>(A||e((A={exports:{}}).exports,A),A.exports);\nvar app=St((exports,module)=>{module.exports=1});";
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(bundle_source.into()),
        ));
        // The module owns only its handle declarator + body, leaving the `St`
        // helper definition outside as a prelude binding.
        let module_span_start = bundle_source
            .find("var app=")
            .expect("fixture contains `var app=`") as u32;
        let module_span_end = bundle_source.len() as u32;
        rows.modules.push(
            ModuleInput::application(ModuleId(10), "esbuild:app", "esbuild:app")
                .with_source_file(1)
                .with_source_span(reverts_input::SourceSpan::new(
                    module_span_start,
                    module_span_end,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let output = enrich_program(ProgramModel::from_input(input));

        assert!(
            !output.audit.findings().iter().any(|finding| {
                finding.code == FindingCode::MissingDefinition
                    && finding.binding.as_deref() == Some("St")
            }),
            "St (prelude-classified CommonJsWrapper) must not be flagged as missing: {:#?}",
            output.audit.findings()
        );
    }

    #[test]
    fn unresolved_ast_write_is_reported_as_missing_definition() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "missing-write.js",
            Some("missing = 1;".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(output.audit.findings().iter().any(|finding| {
            finding.code == FindingCode::MissingDefinition
                && finding.binding.as_deref() == Some("missing")
                && finding.message.contains("written")
        }));
    }

    #[test]
    fn enrich_synthesizes_dependency_edge_from_free_read_to_owner() {
        // Module `a` defines `helper`; module `b` reads it as a free variable
        // with no dependency edge. Enrichment must synthesize the b->a module
        // dependency so the planner emits `import { helper } from './a'`.
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "a.js",
            Some("function helper(){return 1}".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "b.js",
            Some("helper()".to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "a", "a.ts").with_source_file(1));
        rows.modules
            .push(ModuleInput::application(ModuleId(2), "b", "b.ts").with_source_file(2));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);

        let output = enrich_program(model);

        let deps = &output.program.model().input().dependencies;
        assert!(
            deps.iter().any(|dep| dep.from_module_id == ModuleId(2)
                && dep.target == ModuleDependencyTarget::Module(ModuleId(1))),
            "expected synthesized edge b->a, got: {deps:?}"
        );
    }

    #[test]
    fn enrich_does_not_synthesize_edge_for_ambiguous_owner() {
        // Both `a` and `c` define `helper`; `b` reads it. The owner is
        // ambiguous (minified collision shape), so NO edge is synthesized —
        // we never guess which module owns a colliding name.
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "a.js",
            Some("function helper(){return 1}".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "b.js",
            Some("helper()".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            3,
            "c.js",
            Some("function helper(){return 2}".to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "a", "a.ts").with_source_file(1));
        rows.modules
            .push(ModuleInput::application(ModuleId(2), "b", "b.ts").with_source_file(2));
        rows.modules
            .push(ModuleInput::application(ModuleId(3), "c", "c.ts").with_source_file(3));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);

        let output = enrich_program(model);

        let deps = &output.program.model().input().dependencies;
        assert!(
            !deps.iter().any(|dep| dep.from_module_id == ModuleId(2)),
            "ambiguous owner must not synthesize an edge, got: {deps:?}"
        );
    }

    #[test]
    fn enrich_disambiguates_ambiguous_owner_via_reachable_dependency() {
        // `X` is defined in both o1 and o2 (ambiguous by name). Module `b`
        // reads X and already depends on o1. Reachability narrows the owner
        // set to the single reachable module (o1), so the read resolves —
        // without guessing among the colliding names.
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "o1.js",
            Some("function X(){return 1}".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "o2.js",
            Some("function X(){return 2}".to_string()),
        ));
        rows.source_files
            .push(SourceFileInput::new(3, "b.js", Some("X()".to_string())));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "o1", "o1.ts").with_source_file(1));
        rows.modules
            .push(ModuleInput::application(ModuleId(2), "o2", "o2.ts").with_source_file(2));
        rows.modules
            .push(ModuleInput::application(ModuleId(3), "b", "b.ts").with_source_file(3));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(3),
            target: ModuleDependencyTarget::Module(ModuleId(1)),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);

        let output = enrich_program(model);

        let unresolved = output.program.model().graph().def_use().unresolved_reads();
        assert!(
            !unresolved
                .iter()
                .any(|(module, binding)| *module == ModuleId(3) && binding.as_str() == "X"),
            "b's X must resolve to the reachable owner o1: {unresolved:?}"
        );
    }

    #[test]
    fn legal_var_redeclaration_is_not_flagged_as_duplicate() {
        // `var value = 1; var value = 2;` is a single hoisted binding (the
        // def-use graph already dedupes it), and emission preserves `var`
        // (DeclaratorSplit keeps the declaration kind), so this is valid JS.
        // For validly-parsed input a duplicate *top-level* binding can only be
        // var/function hoisting (duplicate let/const is a parse error), so it
        // is always benign — surfacing it is a false positive, not a defect.
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "duplicate.js",
            Some("var value = 1; var value = 2;".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(!output.audit.has(FindingCode::DuplicateTopLevelBinding));
    }

    #[test]
    fn ambient_runtime_globals_do_not_fail_def_use_audit() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "runtime.js",
            Some("console.log(process.cwd());".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(!output.audit.has(FindingCode::MissingDefinition));
    }

    #[test]
    fn ambient_browser_and_extension_globals_do_not_fail_def_use_audit() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "browser-runtime.js",
            Some(
                r#"
                chrome.runtime.sendMessage({ ok: true });
                fetch(new Request("/api"), { headers: new Headers() });
                const blob = new Blob(["ok"]);
                new DOMParser().parseFromString("<p>ok</p>", "text/html");
                localStorage.setItem("blob", URL.createObjectURL(blob));
                "#
                .to_string(),
            ),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        assert!(!output.audit.has(FindingCode::MissingDefinition));
    }

    fn identifiers<const N: usize>(values: [&str; N]) -> BTreeSet<String> {
        values.into_iter().map(str::to_string).collect()
    }

    #[test]
    fn esbuild_wrapper_names_list_covers_known_wrappers() {
        let names: std::collections::BTreeSet<&'static str> =
            super::ESBUILD_WRAPPER_NAMES.iter().copied().collect();
        for name in [
            "__toESM",
            "__toCommonJS",
            "__commonJS",
            "__esm",
            "__defProp",
            "__defProps",
            "__export",
            "__exportStar",
            "__reExport",
            "__copyProps",
        ] {
            assert!(names.contains(name), "missing wrapper name {name}");
        }
    }

    #[test]
    fn enriched_program_contains_function_fingerprints_for_modules_with_source() {
        let mut rows = valid_rows();
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("function add(a, b) { return a + b; }".to_string()),
        ));
        rows.modules[0] =
            ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1);
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let output = enrich_program(ProgramModel::from_input(input));

        let fps = output
            .program
            .function_fingerprints()
            .get(&ModuleId(1))
            .expect("module 1 should have fingerprints");
        assert!(
            !fps.is_empty(),
            "expected at least one function fingerprint"
        );
        assert!(
            fps.iter().any(|fp| fp.param_count == 2),
            "expected the 2-param `add` fn to be fingerprinted",
        );
    }

    #[test]
    fn clean_module_layout_path_recovers_multi_segment_path() {
        // A namespaced `modules/<id>-<layout>.ts` recovers its clean layout path
        // when the remainder is a real multi-segment directory path.
        assert_eq!(
            super::clean_module_layout_path(
                ModuleId(228340),
                "modules/228340-smithy/http-request-handler.ts"
            ),
            Some("smithy/http-request-handler.ts".to_string())
        );
        assert_eq!(
            super::clean_module_layout_path(ModuleId(7), "modules/7-utils/git/gitFilesystem.ts"),
            Some("utils/git/gitFilesystem.ts".to_string())
        );
    }

    #[test]
    fn clean_module_layout_path_keeps_bare_minified_names_namespaced() {
        // A bare/minified name (no directory) must NOT be hoisted to the project
        // root — it stays namespaced.
        assert_eq!(
            super::clean_module_layout_path(ModuleId(229374), "modules/229374-Ov6.ts"),
            None
        );
        // A path with a different module id in the prefix is not stripped.
        assert_eq!(
            super::clean_module_layout_path(ModuleId(1), "modules/2-utils/foo.ts"),
            None
        );
    }

    #[test]
    fn is_safe_ts_module_path_validates_segments_and_extension() {
        assert!(super::is_safe_ts_module_path("utils/git/gitFilesystem.ts"));
        assert!(super::is_safe_ts_module_path("a.tsx"));
        assert!(!super::is_safe_ts_module_path("utils/git/gitFilesystem")); // no extension
        assert!(!super::is_safe_ts_module_path("utils/../escape.ts")); // traversal
        assert!(!super::is_safe_ts_module_path("utils//empty.ts")); // empty segment
    }
}
