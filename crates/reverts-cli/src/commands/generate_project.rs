//! `generate-project-v2` subcommand: load a project bundle from SQLite,
//! run the output pipeline, audit-gate the result, then materialise the
//! TypeScript project (sources, scaffold, assets) under `--output`.

use std::fs;
use std::path::{Component, Path, PathBuf};

use reverts_input::sqlite::load_project_bundle_from_sqlite;
use reverts_pipeline::{EmittedAsset, EmittedFile, RuntimeDependency, generate_project_from_input};

use crate::errors::{CliError, CliRunError};
use crate::{format_audit_findings, next_path, parse_project_id};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerateProjectV2Args {
    pub input: PathBuf,
    pub output: PathBuf,
    pub project_id: u32,
}

impl GenerateProjectV2Args {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut input = None;
        let mut output = None;
        let mut project_id = None;
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == crate::help::GENERATE_PROJECT_V2_COMMAND)
        {
            args.remove(0);
        }
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" => input = Some(next_path(&mut args, "--input")?),
                "--output" => output = Some(next_path(&mut args, "--output")?),
                "--project-id" => {
                    project_id = Some(parse_project_id(crate::next_value(
                        &mut args,
                        "--project-id",
                    )?)?);
                }
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }

        Ok(Self {
            input: input.ok_or(CliError::MissingArgument("--input"))?,
            output: output.ok_or(CliError::MissingArgument("--output"))?,
            project_id: project_id.ok_or(CliError::MissingArgument("--project-id"))?,
        })
    }
}

pub(crate) fn run(args: GenerateProjectV2Args) -> Result<(), CliRunError> {
    let input = load_project_bundle_from_sqlite(&args.input, args.project_id)
        .map_err(CliRunError::LoadInput)?;
    let run = generate_project_from_input(input).map_err(CliRunError::Pipeline)?;

    if !run.audit.is_clean() {
        return Err(CliRunError::AuditRejected(format_audit_findings(
            &run.audit,
        )));
    }

    let written = write_emitted_project(
        &run.project.files,
        &run.assets,
        &args.output,
        &run.runtime_dependencies,
    )?;
    println!(
        "generated project {} into {} with {written} files",
        args.project_id,
        args.output.display()
    );
    Ok(())
}

pub(crate) fn write_emitted_project(
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
        fs::write(path.as_path(), asset.bytes.as_slice()).map_err(|source| {
            CliRunError::WriteOutput {
                path: path.clone(),
                source,
            }
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
    let package_json =
        typescript_package_json(runtime_dependencies, has_cli_entrypoint, !assets.is_empty());
    write_project_file(output, "package.json", package_json.as_str())?;
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

fn typescript_package_json(
    runtime_dependencies: &[RuntimeDependency],
    has_cli_entrypoint: bool,
    has_assets: bool,
) -> String {
    let mut dependencies = serde_json::Map::new();
    for dependency in runtime_dependencies {
        dependencies.insert(
            dependency.package_name.clone(),
            serde_json::Value::String(dependency.package_version.clone()),
        );
    }

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
