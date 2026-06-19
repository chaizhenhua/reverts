//! Accepted-project writer for generated TypeScript projects.
//!
//! This is an adapter boundary: command code decides *when* writing is allowed;
//! this module is the only place that materialises an `AcceptedProject` plus
//! scaffold/assets onto the filesystem.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use reverts_pipeline::{AcceptedProject, EmittedAsset, EmittedFile, RuntimeDependency};
use semver::{Version, VersionReq};

use crate::errors::CliRunError;

pub(crate) fn write_accepted_project(
    project: &AcceptedProject,
    assets: &[EmittedAsset],
    output: &Path,
    runtime_dependencies: &[RuntimeDependency],
) -> Result<usize, CliRunError> {
    write_project_files(
        project.files.as_slice(),
        assets,
        output,
        runtime_dependencies,
    )
}

#[cfg(test)]
pub(crate) fn write_emitted_project(
    files: &[EmittedFile],
    assets: &[EmittedAsset],
    output: &Path,
    runtime_dependencies: &[RuntimeDependency],
) -> Result<usize, CliRunError> {
    write_project_files(files, assets, output, runtime_dependencies)
}

fn write_project_files(
    files: &[EmittedFile],
    assets: &[EmittedAsset],
    output: &Path,
    runtime_dependencies: &[RuntimeDependency],
) -> Result<usize, CliRunError> {
    fs::create_dir_all(output).map_err(|source| CliRunError::WriteOutput {
        path: output.to_path_buf(),
        source,
    })?;
    let has_cli_entrypoint = files.iter().any(|file| file.path == "cli.ts");
    write_typescript_project_scaffold(output, runtime_dependencies, has_cli_entrypoint, assets)?;

    for file in files {
        let path = checked_output_path(output, file.path.as_str())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| CliRunError::WriteOutput {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::write(&path, file.source.as_bytes())
            .map_err(|source| CliRunError::WriteOutput { path, source })?;
    }

    for asset in assets {
        let path = checked_output_path(output, asset.path.as_str())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| CliRunError::WriteOutput {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let bytes = output_asset_bytes(asset);
        fs::write(path.as_path(), bytes.as_slice()).map_err(|source| CliRunError::WriteOutput {
            path: path.clone(),
            source,
        })?;
        set_executable_bit(path.as_path(), asset.executable)?;
    }

    Ok(files.len() + assets.len())
}

fn write_typescript_project_scaffold(
    output: &Path,
    runtime_dependencies: &[RuntimeDependency],
    has_cli_entrypoint: bool,
    assets: &[EmittedAsset],
) -> Result<(), CliRunError> {
    let package_json = typescript_package_json(
        runtime_dependencies,
        has_cli_entrypoint,
        !assets.is_empty(),
        assets,
    );
    write_project_file(output, "package.json", package_json.as_str())?;
    write_package_compat_shims(output, runtime_dependencies)?;
    if should_write_legacy_peer_deps_npmrc(runtime_dependencies) {
        write_project_file(output, ".npmrc", TYPESCRIPT_NPMRC)?;
    }
    write_project_file(output, "tsconfig.json", TYPESCRIPT_TSCONFIG_JSON)?;
    write_project_file(
        output,
        "tsconfig.runtime.json",
        TYPESCRIPT_RUNTIME_TSCONFIG_JSON,
    )?;
    if !assets.is_empty() {
        let copy_assets = typescript_copy_assets_script(assets);
        write_project_file(output, "scripts/copy-assets.mjs", copy_assets.as_str())?;
    }
    Ok(())
}

// Generated projects preserve the package versions proven from the bundled
// source. Modern npm peer-dependency resolution can reject historical
// lockfile combinations even though they are the versions that were bundled,
// so only known generated-output peer conflicts opt into legacy peer semantics.
const TYPESCRIPT_NPMRC: &str = r"legacy-peer-deps=true
";

fn should_write_legacy_peer_deps_npmrc(runtime_dependencies: &[RuntimeDependency]) -> bool {
    let dependency_versions = runtime_dependencies
        .iter()
        .map(|dependency| {
            (
                dependency.package_name.as_str(),
                dependency.package_version.as_str(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    has_unsatisfied_ink_7_peer_dependency(&dependency_versions)
        || has_unsatisfied_anthropic_zod_peer_dependency(&dependency_versions)
}

fn has_unsatisfied_ink_7_peer_dependency(
    dependency_versions: &std::collections::BTreeMap<&str, &str>,
) -> bool {
    let Some(ink_version) = dependency_versions.get("ink").copied() else {
        return false;
    };
    if !version_satisfies(ink_version, ">=7.0.0") {
        return false;
    }
    dependency_versions
        .get("react")
        .copied()
        .is_some_and(|version| !version_satisfies(version, ">=19.2.0"))
        || dependency_versions
            .get("react-devtools-core")
            .copied()
            .is_some_and(|version| !version_satisfies(version, ">=6.1.2"))
}

fn has_unsatisfied_anthropic_zod_peer_dependency(
    dependency_versions: &std::collections::BTreeMap<&str, &str>,
) -> bool {
    if !dependency_versions.contains_key("@anthropic-ai/sdk") {
        return false;
    }
    dependency_versions
        .get("zod")
        .copied()
        .is_some_and(|version| !version_satisfies(version, "^3.25.0 || ^4.0.0"))
}

fn version_satisfies(version: &str, requirement: &str) -> bool {
    let Ok(version) = Version::parse(version.trim()) else {
        return false;
    };
    VersionReq::parse(requirement).is_ok_and(|requirement| requirement.matches(&version))
}

fn typescript_package_json(
    runtime_dependencies: &[RuntimeDependency],
    has_cli_entrypoint: bool,
    has_assets: bool,
    assets: &[EmittedAsset],
) -> String {
    let local_private_packages = local_private_package_dependencies(assets);
    let mut dependencies = serde_json::Map::new();
    for dependency in runtime_dependencies {
        let specifier = local_private_packages
            .get(dependency.package_name.as_str())
            .cloned()
            .unwrap_or_else(|| dependency.package_version.clone());
        dependencies.insert(
            dependency.package_name.clone(),
            serde_json::Value::String(specifier),
        );
    }
    for shim in package_compat_shims(runtime_dependencies) {
        dependencies.insert(
            shim.package_name.to_string(),
            serde_json::Value::String(format!("file:./vendor-shims/{}", shim.package_name)),
        );
        dependencies.insert(
            shim.alias_name.to_string(),
            serde_json::Value::String(format!(
                "npm:{}@{}",
                shim.package_name, shim.package_version
            )),
        );
    }
    add_known_runtime_peer_dependencies(&mut dependencies);

    let mut scripts = serde_json::Map::new();
    scripts.insert(
        "check".to_string(),
        serde_json::Value::String("tsc --noEmit -p tsconfig.json".to_string()),
    );
    scripts.insert(
        "build".to_string(),
        serde_json::Value::String(if has_assets {
            "tsc -p tsconfig.runtime.json && node ./scripts/copy-assets.mjs".to_string()
        } else {
            "tsc -p tsconfig.runtime.json".to_string()
        }),
    );
    if has_cli_entrypoint {
        scripts.insert(
            "start".to_string(),
            serde_json::Value::String("node ./dist/cli.js".to_string()),
        );
    }

    let mut package = serde_json::json!({
        "name": "reverts-output",
        "version": "0.0.0",
        "private": true,
        "type": "module",
        "description": "Decompiled TypeScript source generated by Reverts",
        "scripts": scripts,
        "dependencies": dependencies,
        "devDependencies": {
            "@types/node": "*",
            "typescript": "^5",
            "tsx": "^4",
        },
    });
    if has_cli_entrypoint {
        package["bin"] = serde_json::json!({
            "reverts-output": "./dist/cli.js",
        });
    }
    serde_json::to_string_pretty(&package).expect("package.json scaffold is serializable") + "\n"
}

fn add_known_runtime_peer_dependencies(
    dependencies: &mut serde_json::Map<String, serde_json::Value>,
) {
    if dependencies.contains_key("@sentry/node")
        || dependencies.contains_key("@sentry/opentelemetry")
    {
        insert_dependency_if_absent(
            dependencies,
            "@opentelemetry/context-async-hooks",
            "^1.30.1",
        );
        insert_dependency_if_absent(dependencies, "@opentelemetry/instrumentation", "^0.57.1");
    }
}

fn insert_dependency_if_absent(
    dependencies: &mut serde_json::Map<String, serde_json::Value>,
    package_name: &str,
    package_version: &str,
) {
    dependencies
        .entry(package_name.to_string())
        .or_insert_with(|| serde_json::Value::String(package_version.to_string()));
}

fn local_private_package_dependencies(assets: &[EmittedAsset]) -> BTreeMap<String, String> {
    let mut dependencies = BTreeMap::new();
    for asset in assets {
        let Some(package_dir) = asset
            .path
            .strip_prefix("assets/node_modules/")
            .and_then(|path| path.strip_suffix("/package.json"))
        else {
            continue;
        };
        let Some(package_json) = local_private_package_manifest(asset) else {
            continue;
        };
        let Some(package_name) = package_json.get("name").and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        if package_name != package_dir {
            continue;
        }
        dependencies.insert(
            package_name.to_string(),
            format!("file:./assets/node_modules/{package_name}"),
        );
    }
    dependencies
}

fn is_valid_local_package_name(package_name: &str) -> bool {
    if package_name.is_empty() || package_name.contains("..") {
        return false;
    }
    let segments = package_name.split('/').collect::<Vec<_>>();
    if package_name.starts_with('@') {
        segments.len() == 2
            && segments
                .iter()
                .all(|segment| !segment.is_empty() && is_valid_npm_path_segment(segment))
    } else {
        segments.len() == 1 && is_valid_npm_path_segment(segments[0])
    }
}

fn is_valid_npm_path_segment(segment: &str) -> bool {
    segment
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'.' | b'-' | b'_'))
}

fn output_asset_bytes(asset: &EmittedAsset) -> Vec<u8> {
    let Some(mut package_json) = local_private_package_manifest(asset) else {
        return asset.bytes.clone();
    };
    remove_workspace_protocol_dependencies(&mut package_json);
    let Ok(mut bytes) = serde_json::to_vec_pretty(&package_json) else {
        return asset.bytes.clone();
    };
    bytes.push(b'\n');
    bytes
}

fn local_private_package_manifest(asset: &EmittedAsset) -> Option<serde_json::Value> {
    let package_dir = asset
        .path
        .strip_prefix("assets/node_modules/")
        .and_then(|path| path.strip_suffix("/package.json"))?;
    let package_json = serde_json::from_slice::<serde_json::Value>(&asset.bytes).ok()?;
    if package_json
        .get("private")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }
    let package_name = package_json
        .get("name")
        .and_then(serde_json::Value::as_str)?;
    if package_name != package_dir || !is_valid_local_package_name(package_name) {
        return None;
    }
    Some(package_json)
}

fn remove_workspace_protocol_dependencies(package_json: &mut serde_json::Value) {
    for field in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        let Some(dependencies) = package_json
            .get_mut(field)
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        dependencies.retain(|_name, version| {
            !version
                .as_str()
                .is_some_and(|version| version.trim().starts_with("workspace:"))
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackageCompatShim<'a> {
    package_name: &'a str,
    package_version: &'a str,
    alias_name: &'a str,
}

fn package_compat_shims(runtime_dependencies: &[RuntimeDependency]) -> Vec<PackageCompatShim<'_>> {
    runtime_dependencies
        .iter()
        .filter_map(|dependency| match dependency.package_name.as_str() {
            "react" => Some(PackageCompatShim {
                package_name: "react",
                package_version: dependency.package_version.as_str(),
                alias_name: "react-cjs",
            }),
            "react-dom" => Some(PackageCompatShim {
                package_name: "react-dom",
                package_version: dependency.package_version.as_str(),
                alias_name: "react-dom-cjs",
            }),
            _ => None,
        })
        .collect()
}

fn write_package_compat_shims(
    output: &Path,
    runtime_dependencies: &[RuntimeDependency],
) -> Result<(), CliRunError> {
    for shim in package_compat_shims(runtime_dependencies) {
        match shim.package_name {
            "react" => write_react_compat_shim(output)?,
            "react-dom" => write_react_dom_compat_shim(output)?,
            _ => {}
        }
    }
    Ok(())
}

fn write_react_compat_shim(output: &Path) -> Result<(), CliRunError> {
    write_project_file(
        output,
        "vendor-shims/react/package.json",
        REACT_COMPAT_PACKAGE_JSON,
    )?;
    write_project_file(output, "vendor-shims/react/index.js", REACT_COMPAT_INDEX_JS)?;
    write_project_file(
        output,
        "vendor-shims/react/jsx-runtime.js",
        REACT_COMPAT_JSX_RUNTIME_JS,
    )?;
    write_project_file(
        output,
        "vendor-shims/react/jsx-dev-runtime.js",
        REACT_COMPAT_JSX_DEV_RUNTIME_JS,
    )?;
    Ok(())
}

fn write_react_dom_compat_shim(output: &Path) -> Result<(), CliRunError> {
    write_project_file(
        output,
        "vendor-shims/react-dom/package.json",
        REACT_DOM_COMPAT_PACKAGE_JSON,
    )?;
    write_project_file(
        output,
        "vendor-shims/react-dom/index.js",
        REACT_DOM_COMPAT_INDEX_JS,
    )?;
    write_project_file(
        output,
        "vendor-shims/react-dom/client.js",
        REACT_DOM_COMPAT_CLIENT_JS,
    )?;
    write_project_file(
        output,
        "vendor-shims/react-dom/server.js",
        REACT_DOM_COMPAT_SERVER_JS,
    )?;
    Ok(())
}

const REACT_COMPAT_PACKAGE_JSON: &str = r#"{
  "name": "react",
  "version": "19.2.0",
  "private": true,
  "type": "module",
  "main": "./index.js",
  "exports": {
    ".": "./index.js",
    "./jsx-runtime": "./jsx-runtime.js",
    "./jsx-dev-runtime": "./jsx-dev-runtime.js"
  }
}
"#;

const REACT_COMPAT_INDEX_JS: &str = r#"import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const React = require('react-cjs');

export default React;
export const Children = React.Children;
export const Component = React.Component;
export const Fragment = React.Fragment;
export const Profiler = React.Profiler;
export const PureComponent = React.PureComponent;
export const StrictMode = React.StrictMode;
export const Suspense = React.Suspense;
export const __CLIENT_INTERNALS_DO_NOT_USE_OR_WARN_USERS_THEY_CANNOT_UPGRADE = React.__CLIENT_INTERNALS_DO_NOT_USE_OR_WARN_USERS_THEY_CANNOT_UPGRADE;
export const __COMPILER_RUNTIME = React.__COMPILER_RUNTIME;
export const act = React.act;
export const cache = React.cache;
export const captureOwnerStack = React.captureOwnerStack;
export const cloneElement = React.cloneElement;
export const createContext = React.createContext;
export const createElement = React.createElement;
export const createRef = React.createRef;
export const forwardRef = React.forwardRef;
export const isValidElement = React.isValidElement;
export const lazy = React.lazy;
export const memo = React.memo;
export const startTransition = React.startTransition;
export const unstable_useCacheRefresh = React.unstable_useCacheRefresh;
export const use = React.use;
export const useActionState = React.useActionState;
export const useCallback = React.useCallback;
export const useContext = React.useContext;
export const useDebugValue = React.useDebugValue;
export const useDeferredValue = React.useDeferredValue;
export const useEffect = React.useEffect;
export const useEffectEvent = React.useEffectEvent ?? ((handler) => handler);
export const useId = React.useId;
export const useImperativeHandle = React.useImperativeHandle;
export const useInsertionEffect = React.useInsertionEffect;
export const useLayoutEffect = React.useLayoutEffect;
export const useMemo = React.useMemo;
export const useOptimistic = React.useOptimistic;
export const useReducer = React.useReducer;
export const useRef = React.useRef;
export const useState = React.useState;
export const useSyncExternalStore = React.useSyncExternalStore;
export const useTransition = React.useTransition;
export const version = React.version;
"#;

const REACT_COMPAT_JSX_RUNTIME_JS: &str = r#"import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const runtime = require('react-cjs/jsx-runtime');

export default runtime;
export const Fragment = runtime.Fragment;
export const jsx = runtime.jsx;
export const jsxs = runtime.jsxs;
"#;

const REACT_COMPAT_JSX_DEV_RUNTIME_JS: &str = r#"import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const runtime = require('react-cjs/jsx-dev-runtime');

export default runtime;
export const Fragment = runtime.Fragment;
export const jsxDEV = runtime.jsxDEV;
"#;

const REACT_DOM_COMPAT_PACKAGE_JSON: &str = r#"{
  "name": "react-dom",
  "version": "19.2.0",
  "private": true,
  "type": "module",
  "main": "./index.js",
  "exports": {
    ".": "./index.js",
    "./client": "./client.js",
    "./server": "./server.js"
  }
}
"#;

const REACT_DOM_COMPAT_INDEX_JS: &str = r#"import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
let cached;
const load = () => (cached ??= require('react-dom-cjs'));
const proxy = new Proxy({}, {
  get(_target, property) {
    return load()[property];
  },
  has(_target, property) {
    return property in load();
  },
  ownKeys() {
    return Reflect.ownKeys(load());
  },
  getOwnPropertyDescriptor(_target, property) {
    const descriptor = Object.getOwnPropertyDescriptor(load(), property);
    return descriptor ? { ...descriptor, configurable: true } : undefined;
  }
});

export default proxy;
export const createPortal = (...args) => load().createPortal(...args);
export const flushSync = (...args) => load().flushSync(...args);
export const preconnect = (...args) => load().preconnect?.(...args);
export const prefetchDNS = (...args) => load().prefetchDNS?.(...args);
export const preinit = (...args) => load().preinit?.(...args);
export const preinitModule = (...args) => load().preinitModule?.(...args);
export const preload = (...args) => load().preload?.(...args);
export const preloadModule = (...args) => load().preloadModule?.(...args);
export const requestFormReset = (...args) => load().requestFormReset?.(...args);
export const unstable_batchedUpdates = (...args) => load().unstable_batchedUpdates?.(...args);
export const useFormState = (...args) => load().useFormState?.(...args);
export const useFormStatus = (...args) => load().useFormStatus?.(...args);
"#;

const REACT_DOM_COMPAT_CLIENT_JS: &str = r#"import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
let cached;
const load = () => (cached ??= require('react-dom-cjs/client'));
const proxy = new Proxy({}, {
  get(_target, property) {
    return load()[property];
  }
});

export default proxy;
export const createRoot = (...args) => load().createRoot(...args);
export const hydrateRoot = (...args) => load().hydrateRoot(...args);
"#;

const REACT_DOM_COMPAT_SERVER_JS: &str = r#"import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
let cached;
const load = () => (cached ??= require('react-dom-cjs/server'));
const proxy = new Proxy({}, {
  get(_target, property) {
    return load()[property];
  }
});

export default proxy;
export const renderToNodeStream = (...args) => load().renderToNodeStream?.(...args);
export const renderToPipeableStream = (...args) => load().renderToPipeableStream?.(...args);
export const renderToReadableStream = (...args) => load().renderToReadableStream?.(...args);
export const renderToStaticMarkup = (...args) => load().renderToStaticMarkup?.(...args);
export const renderToStaticNodeStream = (...args) => load().renderToStaticNodeStream?.(...args);
export const renderToString = (...args) => load().renderToString?.(...args);
"#;

fn typescript_copy_assets_script(assets: &[EmittedAsset]) -> String {
    let manifest = assets
        .iter()
        .map(|asset| {
            serde_json::json!({
                "from": asset.path.as_str(),
                "to": format!("dist/{}", asset.path),
                "executable": asset.executable,
            })
        })
        .collect::<Vec<_>>();
    let manifest_json =
        serde_json::to_string_pretty(&manifest).expect("asset manifest is serializable");
    format!(
        r#"import {{ chmodSync, copyFileSync, mkdirSync }} from 'node:fs';
import {{ dirname, join }} from 'node:path';
import {{ fileURLToPath }} from 'node:url';

const assets = {manifest_json};
const projectRoot = dirname(dirname(fileURLToPath(import.meta.url)));

for (const asset of assets) {{
  const from = join(projectRoot, asset.from);
  const to = join(projectRoot, asset.to);
  mkdirSync(dirname(to), {{ recursive: true }});
  copyFileSync(from, to);
  if (asset.executable) {{
    chmodSync(to, 0o755);
  }}
}}
"#
    )
}

fn write_project_file(output: &Path, relative: &str, source: &str) -> Result<(), CliRunError> {
    let path = checked_output_path(output, relative)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| CliRunError::WriteOutput {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(path.as_path(), source.as_bytes())
        .map_err(|source| CliRunError::WriteOutput { path, source })
}

#[cfg(unix)]
fn set_executable_bit(path: &Path, executable: bool) -> Result<(), CliRunError> {
    use std::os::unix::fs::PermissionsExt;

    if !executable {
        return Ok(());
    }
    let metadata = fs::metadata(path).map_err(|source| CliRunError::WriteOutput {
        path: path.to_path_buf(),
        source,
    })?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions).map_err(|source| CliRunError::WriteOutput {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn set_executable_bit(_path: &Path, _executable: bool) -> Result<(), CliRunError> {
    Ok(())
}

const TYPESCRIPT_TSCONFIG_JSON: &str = r#"{
  "compilerOptions": {
    "allowJs": false,
    "esModuleInterop": true,
    "forceConsistentCasingInFileNames": true,
    "jsx": "react-jsx",
    "lib": [
      "es2024",
      "dom",
      "esnext"
    ],
    "module": "ES2022",
    "moduleResolution": "bundler",
    "noEmit": true,
    "noImplicitAny": false,
    "resolveJsonModule": true,
    "skipLibCheck": true,
    "strict": false,
    "target": "ES2022"
  },
  "include": [
    "cli.ts",
    "modules/**/*.ts",
    "modules/**/*.tsx",
    "**/*.d.ts"
  ]
}
"#;

const TYPESCRIPT_RUNTIME_TSCONFIG_JSON: &str = r#"{
  "extends": "./tsconfig.json",
  "compilerOptions": {
    "declaration": false,
    "declarationMap": false,
    "emitDeclarationOnly": false,
    "noEmit": false,
    "noEmitOnError": true,
    "outDir": "dist",
    "rootDir": ".",
    "sourceMap": false
  }
}
"#;

pub(crate) fn checked_output_path(output: &Path, relative: &str) -> Result<PathBuf, CliRunError> {
    let relative = Path::new(relative);
    if relative.is_absolute() {
        return Err(CliRunError::UnsafeOutputPath(relative.to_path_buf()));
    }

    let mut path = output.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CliRunError::UnsafeOutputPath(relative.to_path_buf()));
            }
        }
    }
    Ok(path)
}
