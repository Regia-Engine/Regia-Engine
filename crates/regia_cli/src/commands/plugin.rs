// SPDX-License-Identifier: MPL-2.0
//! `regia plugin` (`docs/specs/57-plugin-extension-api.md` § 3.1, ADR-1885): the project's
//! extensions from the command line, the same resolver and the same `Regia.toml` edits the
//! `plugin.*` agent methods use.
//!
//! - `list`, `info <id>`: what discovery found and what the resolver made of it;
//! - `check`: the resolver as a dry run, plus the checks that need the crates on disk
//!   (`layer_violation`, `package_include`, `manifest_not_linked`, `linked_not_enabled`); exit 1 on
//!   any error;
//! - `enable <id>`, `disable <id>`: edit `[extensions]`, keeping comments; `enable` refuses a
//!   capability that was not granted (`--grant`, `--grant-all`);
//! - `sync`: writes the committed `src/extensions_generated.rs` list of linked crates;
//! - `build <id>`: builds a source extension as a stamped dynamic library for the editor;
//! - `new <id>`: scaffolds an extension under `extensions/`;
//! - `tool <id> <name>`: runs a `[[tool]]` helper as a bounded child process.
//!
//! Every subcommand takes `--project` (default `.`) and `--json`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use regia_agent::plugins::{diagnostics_json, dry_run, reserved_namespaces};
use regia_asset_pipeline::extension_cook::{ExtensionCookOptions, ExtensionRoot, cook_extension_content};
use regia_framework::extension::cooked::{COOKED_ASSET_PATH, COOKED_FILE, CookedExtensions};
use regia_framework::extension::discover::{self, MANIFEST_FILE};
use regia_framework::extension::manifest::{CodeEntry, IMPLICIT_CAPABILITIES, KNOWN_CAPABILITIES};
use regia_framework::extension::project::{self, ExtensionsConfig};
use regia_framework::extension::registry::{content_root, host_platform};
use regia_framework::extension::stamp::{self, BUILD_TARGET, dylib_extension};
use regia_framework::extension::{
    Diagnostic, DiscoveredManifest, ExtensionRegistry, ExtensionState, RegistryInputs, ResolvedSet, Severity,
};
use serde_json::{Value, json};

/// The generated list of linked extension crates, relative to the project.
pub const GENERATED_FILE: &str = "src/extensions_generated.rs";

/// Arguments every subcommand shares.
#[derive(Debug, Args)]
pub struct ProjectArgs {
    /// Project root (the directory holding `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print JSON on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia plugin` subcommands.
#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// List discovered extensions with state, version and kind.
    List(ListArgs),
    /// Show one extension: manifest, capabilities, dependencies, diagnostics.
    Info(IdArgs),
    /// Run the resolver as a dry run; exit 1 on any error.
    Check(CheckArgs),
    /// Enable an extension in Regia.toml, granting capabilities.
    Enable(EnableArgs),
    /// Disable an extension in Regia.toml.
    Disable(DisableArgs),
    /// Write src/extensions_generated.rs from the enabled source extensions.
    Sync(SyncArgs),
    /// Build a source extension as a stamped dynamic library for the editor.
    Build(BuildArgs),
    /// Scaffold a new extension under extensions/.
    New(NewArgs),
    /// Run one of an extension's [[tool]] helpers as a child process.
    Tool(ToolArgs),
}

/// `regia plugin list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(flatten)]
    project: ProjectArgs,
    /// Only this state: active, failed, filtered, disabled or available.
    #[arg(long)]
    pub state: Option<String>,
}

/// `regia plugin info`.
#[derive(Debug, Args)]
pub struct IdArgs {
    /// The extension id.
    pub id: String,
    #[command(flatten)]
    project: ProjectArgs,
}

/// `regia plugin check`.
#[derive(Debug, Args)]
pub struct CheckArgs {
    #[command(flatten)]
    project: ProjectArgs,
    /// linux, windows, android or macos (default: this host).
    #[arg(long)]
    pub platform: Option<String>,
    /// dev or shipping.
    #[arg(long, default_value = "dev")]
    pub profile: String,
}

/// `regia plugin enable`.
#[derive(Debug, Args)]
pub struct EnableArgs {
    /// The extension id.
    pub id: String,
    #[command(flatten)]
    project: ProjectArgs,
    /// A capability to grant (repeatable).
    #[arg(long = "grant")]
    pub grant: Vec<String>,
    /// Grant everything the manifest requests. In-process code is trusted native code either way.
    #[arg(long, default_value_t = false)]
    pub grant_all: bool,
    /// Edit [extensions.platform.<os>] instead of the base lists.
    #[arg(long)]
    pub platform: Option<String>,
}

/// `regia plugin disable`.
#[derive(Debug, Args)]
pub struct DisableArgs {
    /// The extension id.
    pub id: String,
    #[command(flatten)]
    project: ProjectArgs,
    /// Edit [extensions.platform.<os>] instead of the base lists.
    #[arg(long)]
    pub platform: Option<String>,
}

/// `regia plugin sync`.
#[derive(Debug, Args)]
pub struct SyncArgs {
    #[command(flatten)]
    project: ProjectArgs,
}

/// `regia plugin build`.
#[derive(Debug, Args)]
pub struct BuildArgs {
    /// The extension id.
    pub id: String,
    #[command(flatten)]
    project: ProjectArgs,
    /// Build the release profile (the editor that loads it must be a release build).
    #[arg(long, default_value_t = false)]
    pub release: bool,
    /// Cargo target directory; the editor and the extension must share it.
    #[arg(long)]
    pub target_dir: Option<PathBuf>,
}

/// `regia plugin new`.
#[derive(Debug, Args)]
pub struct NewArgs {
    /// The new extension's id.
    pub id: String,
    #[command(flatten)]
    project: ProjectArgs,
    /// A content-only extension: a manifest and a `content/` directory, no code.
    #[arg(long, default_value_t = false)]
    pub content: bool,
}

/// `regia plugin tool`.
#[derive(Debug, Args)]
pub struct ToolArgs {
    /// The extension id.
    pub id: String,
    /// The tool's name in the manifest.
    pub name: String,
    #[command(flatten)]
    project: ProjectArgs,
    /// Arguments for the tool.
    #[arg(last = true)]
    pub args: Vec<String>,
}

/// Runs a subcommand. Returns `false` when `check` found an error.
///
/// # Errors
///
/// Unreadable project files, refused edits, failed builds.
pub fn run(command: &PluginCommand) -> Result<bool> {
    match command {
        PluginCommand::List(args) => {
            let list = list_json(&args.project.project, args.state.as_deref())?;
            print_or(&args.project, &list, print_list);
            Ok(true)
        }
        PluginCommand::Info(args) => {
            let info = load(&args.project.project)?
                .info_json(&args.id)
                .with_context(|| format!("no extension `{}` was discovered", args.id))?;
            print_or(&args.project, &info, |value| {
                println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
            });
            Ok(true)
        }
        PluginCommand::Check(args) => {
            let report = check(&args.project.project, args.platform.as_deref(), &args.profile)?;
            print_or(&args.project, &report, print_check);
            Ok(report["ok"] == true)
        }
        PluginCommand::Enable(args) => {
            let answer = enable(
                &args.project.project,
                &args.id,
                &args.grant,
                args.grant_all,
                args.platform.as_deref(),
            )?;
            print_or(&args.project, &answer, |value| {
                println!("enabled {}; {}", args.id, restart_line(value));
            });
            Ok(true)
        }
        PluginCommand::Disable(args) => {
            let answer = disable(&args.project.project, &args.id, args.platform.as_deref())?;
            print_or(&args.project, &answer, |value| {
                println!("disabled {}; {}", args.id, restart_line(value));
            });
            Ok(true)
        }
        PluginCommand::Sync(args) => {
            let answer = sync(&args.project.project)?;
            print_or(&args.project, &answer, |value| {
                println!("wrote {GENERATED_FILE}: {}", value["linked"]);
                if value["cargo_missing"].as_array().is_some_and(|m| !m.is_empty()) {
                    println!("add to Cargo.toml [dependencies]: {}", value["cargo_missing"]);
                }
            });
            Ok(true)
        }
        PluginCommand::Build(args) => {
            let answer = build(
                &args.project.project,
                &args.id,
                args.release,
                args.target_dir.as_deref(),
            )?;
            print_or(&args.project, &answer, |value| {
                println!("built {}", value["libraries"]);
            });
            Ok(true)
        }
        PluginCommand::New(args) => {
            let dir = new_extension(&args.project.project, &args.id, args.content)?;
            let answer = json!({ "id": args.id, "path": dir.display().to_string() });
            print_or(&args.project, &answer, |value| {
                println!("created {}", value["path"])
            });
            Ok(true)
        }
        PluginCommand::Tool(args) => {
            let answer = run_tool(&args.project.project, &args.id, &args.name, args.args.clone())?;
            print_or(&args.project, &answer, |value| {
                print!("{}", value["stdout"].as_str().unwrap_or_default());
                eprint!("{}", value["stderr"].as_str().unwrap_or_default());
            });
            Ok(answer["status"] == 0)
        }
    }
}

fn print_or(args: &ProjectArgs, value: &Value, human: impl FnOnce(&Value)) {
    if args.json {
        println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
    } else {
        human(value);
    }
}

fn restart_line(answer: &Value) -> &'static str {
    if answer["restart_required"] == true {
        "the change applies at the next start"
    } else {
        "the change applies at once"
    }
}

fn print_list(list: &Value) {
    for row in list["extensions"].as_array().map_or(&[][..], Vec::as_slice) {
        println!(
            "{:<24} {:<10} {:<10} {:<8} {}",
            row["id"].as_str().unwrap_or_default(),
            row["version"].as_str().unwrap_or_default(),
            row["state"].as_str().unwrap_or_default(),
            row["kind"].as_str().unwrap_or_default(),
            row["diagnostics"]
                .as_array()
                .map(|codes| codes
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(","))
                .unwrap_or_default(),
        );
    }
}

fn print_check(report: &Value) {
    let order: Vec<&str> = report["order"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();
    println!("order: {}", order.join(" "));
    for row in report["diagnostics"].as_array().map_or(&[][..], Vec::as_slice) {
        println!(
            "{}: {} [{}]: {}",
            row["severity"].as_str().unwrap_or_default(),
            row["id"].as_str().unwrap_or_default(),
            row["code"].as_str().unwrap_or_default(),
            row["message"].as_str().unwrap_or_default(),
        );
    }
}

fn manifest_text(project: &Path) -> Result<String> {
    let path = project.join(crate::manifest::MANIFEST_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| path.display().to_string()),
    }
}

fn config(project: &Path) -> Result<ExtensionsConfig> {
    ExtensionsConfig::from_project_toml(&manifest_text(project)?).map_err(Into::into)
}

/// The project's registry as a `tool` host, with every protocol namespace reserved.
fn load(project: &Path) -> Result<ExtensionRegistry> {
    let config = config(project)?;
    let roots = discover::project_roots(project, &config, None);
    let mut inputs =
        RegistryInputs::for_host(Some(project.to_path_buf()), config, discover::discover(&roots));
    inputs.audience = "tool".to_owned();
    inputs.profile = "dev".to_owned();
    inputs.reserved_ids = reserved_namespaces();
    Ok(ExtensionRegistry::resolve_from(inputs))
}

/// `plugin list`.
///
/// # Errors
///
/// `Regia.toml` cannot be read.
pub fn list_json(project: &Path, state: Option<&str>) -> Result<Value> {
    if let Some(state) = state
        && !matches!(state, "active" | "failed" | "filtered" | "disabled" | "available")
    {
        bail!("--state must be active, failed, filtered, disabled or available");
    }
    Ok(load(project)?.list_json(state))
}

fn crate_dir(dir: &Path, entry: &CodeEntry) -> Option<PathBuf> {
    let name = entry.crate_name.as_deref()?;
    [dir.to_path_buf(), dir.join(name)]
        .into_iter()
        .find(|d| d.join("Cargo.toml").is_file())
}

fn crate_table(dir: &Path) -> Option<toml::Table> {
    std::fs::read_to_string(dir.join("Cargo.toml")).ok()?.parse().ok()
}

fn error_diag(id: &str, code: &'static str, message: String) -> Diagnostic {
    Diagnostic {
        id: id.to_owned(),
        code,
        severity: Severity::Error,
        message,
    }
}

/// Checks that need the extension's crates on disk (spec § 2.5 `layer_violation`, § 4.1
/// `package.include`).
#[must_use]
pub fn crate_diagnostics(found: &DiscoveredManifest) -> Vec<Diagnostic> {
    let id = &found.manifest.id;
    let dir = PathBuf::from(&found.path);
    let mut out = Vec::new();
    for entry in &found.manifest.code {
        let Some(table) = crate_dir(&dir, entry).and_then(|d| crate_table(&d)) else {
            continue;
        };
        let runtime = entry.audience.iter().any(|a| a == "game" || a == "server");
        let dependencies = table.get("dependencies").and_then(toml::Value::as_table);
        for forbidden in ["regia_editor", "regia_asset_pipeline"] {
            if runtime && dependencies.is_some_and(|deps| deps.contains_key(forbidden)) {
                out.push(error_diag(
                    id,
                    "layer_violation",
                    format!(
                        "crate `{}` has a game or server audience and depends on {forbidden}; split it into a runtime crate and an editor crate",
                        entry.crate_name.as_deref().unwrap_or_default()
                    ),
                ));
            }
        }
        let include = table
            .get("package")
            .and_then(|package| package.get("include"))
            .and_then(toml::Value::as_array);
        if let Some(include) = include
            && !include.iter().any(|item| item.as_str() == Some(MANIFEST_FILE))
        {
            out.push(Diagnostic {
                id: id.clone(),
                code: "package_include",
                severity: Severity::Warning,
                message: format!("the crate's package.include does not list {MANIFEST_FILE}"),
            });
        }
    }
    out
}

/// The crates `src/extensions_generated.rs` links, or `None` when the file does not exist.
#[must_use]
pub fn generated_crates(project: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(project.join(GENERATED_FILE)).ok()?;
    let marker = ["crates = [", "register_extensions!["]
        .into_iter()
        .find(|marker| text.contains(marker))?;
    // From the end: `crates` is written last, after manifests whose text could hold anything.
    let start = text.rfind(marker)? + marker.len();
    let end = start + text[start..].find(']')?;
    Some(
        text[start..end]
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// The cargo crate names of the source entries a game links, as Rust identifiers, sorted.
fn linked_crates(registry: &ExtensionRegistry, set: &ResolvedSet) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for resolved in &set.order {
        let Some(found) = registry.discovered(&resolved.id) else {
            continue;
        };
        for entry in &found.manifest.code {
            if entry.audience.iter().any(|a| a == "game")
                && let Some(name) = &entry.crate_name
            {
                out.push((resolved.id.clone(), name.replace('-', "_")));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn link_diagnostics(project: &Path, registry: &ExtensionRegistry, set: &ResolvedSet) -> Vec<Diagnostic> {
    let Some(generated) = generated_crates(project) else {
        return Vec::new();
    };
    let expected = linked_crates(registry, set);
    let mut out = Vec::new();
    for (id, name) in &expected {
        if !generated.contains(name) {
            out.push(error_diag(
                id,
                "manifest_not_linked",
                format!("`{name}` is enabled but not in {GENERATED_FILE}; run `regia plugin sync`"),
            ));
        }
    }
    for name in &generated {
        if !expected.iter().any(|(_, crate_name)| crate_name == name) {
            out.push(Diagnostic {
                id: name.clone(),
                code: "linked_not_enabled",
                severity: Severity::Warning,
                message: format!("`{name}` is in {GENERATED_FILE} but not enabled; run `regia plugin sync`"),
            });
        }
    }
    out
}

/// `plugin check`: `{ ok, order, diagnostics, states, resolved_hash, platform, profile }`.
///
/// # Errors
///
/// `Regia.toml` cannot be read.
pub fn check(project: &Path, platform: Option<&str>, profile: &str) -> Result<Value> {
    if let Some(platform) = platform
        && !matches!(platform, "linux" | "windows" | "android" | "macos")
    {
        bail!("--platform must be linux, windows, android or macos");
    }
    if !matches!(profile, "dev" | "shipping") {
        bail!("--profile must be dev or shipping");
    }
    let registry = load(project)?;
    let platform = platform.unwrap_or(host_platform());
    let set = dry_run(&registry, registry.config(), platform, profile);
    let mut diagnostics: Vec<Diagnostic> = registry.inputs().discovery.diagnostics.clone();
    diagnostics.extend(set.diagnostics.iter().cloned());
    for resolved in &set.order {
        if let Some(found) = registry.discovered(&resolved.id) {
            diagnostics.extend(crate_diagnostics(found));
        }
    }
    diagnostics.extend(link_diagnostics(project, &registry, &set));
    let ok = diagnostics.iter().all(|d| d.severity != Severity::Error);
    Ok(json!({
        "ok": ok,
        "order": set.order.iter().map(|e| json!({ "id": e.id, "version": e.version.to_string(), "phase": e.phase.name() })).collect::<Vec<_>>(),
        "diagnostics": diagnostics_json(&diagnostics),
        "states": set.states.iter().map(|(id, state)| (id.clone(), json!(state.name()))).collect::<serde_json::Map<_, _>>(),
        "resolved_hash": set.resolved_hash,
        "platform": platform,
        "profile": profile,
    }))
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| tmp.display().to_string())?;
    std::fs::rename(&tmp, path).with_context(|| path.display().to_string())
}

/// `plugin enable`.
///
/// # Errors
///
/// Unknown id, unknown capability, a capability not granted, or an edit that cannot be done.
pub fn enable(
    project: &Path,
    id: &str,
    grant: &[String],
    grant_all: bool,
    platform: Option<&str>,
) -> Result<Value> {
    let registry = load(project)?;
    let found = registry
        .discovered(id)
        .with_context(|| format!("no extension `{id}` was discovered"))?;
    if let Some(bad) = grant.iter().find(|g| !KNOWN_CAPABILITIES.contains(&g.as_str())) {
        bail!("unknown_capability: `{bad}` is not a capability");
    }
    let requested: Vec<String> = found
        .manifest
        .capabilities
        .iter()
        .filter(|c| !IMPLICIT_CAPABILITIES.contains(&c.as_str()))
        .cloned()
        .collect();
    // Only what the manifest asks for is stored, as `plugin.enable` does.
    let granted: Vec<String> = if grant_all {
        requested.clone()
    } else {
        grant.iter().filter(|g| requested.contains(g)).cloned().collect()
    };
    let already = registry.config().grant_map().remove(id).unwrap_or_default();
    // A capability only a `[[tool]]` lists gates that tool, so `--grant-all` covers it and nothing
    // requires it.
    let load = &found.manifest.load_capabilities;
    let missing: Vec<&String> = requested
        .iter()
        .filter(|c| load.contains(c) && !already.contains(c) && !granted.contains(c))
        .collect();
    if !missing.is_empty() {
        bail!(
            "capability_not_granted: `{id}` requests {missing:?}; review them with `regia plugin info {id}` and pass --grant or --grant-all"
        );
    }
    let text = manifest_text(project)?;
    let mut edited = project::set_enabled(&text, id, true, platform)?;
    if !granted.is_empty() {
        edited = project::set_grants(&edited, id, &granted)?;
    }
    write_atomic(&project.join(crate::manifest::MANIFEST_FILE), &edited)?;
    Ok(json!({ "enabled": true, "restart_required": !found.manifest.code.is_empty() }))
}

/// `plugin disable`.
///
/// # Errors
///
/// Unknown id, or an edit that cannot be done.
pub fn disable(project: &Path, id: &str, platform: Option<&str>) -> Result<Value> {
    let registry = load(project)?;
    let found = registry
        .discovered(id)
        .with_context(|| format!("no extension `{id}` was discovered"))?;
    let text = manifest_text(project)?;
    let edited = project::set_enabled(&text, id, false, platform)?;
    let config = ExtensionsConfig::from_project_toml(&edited)?;
    let platform_name = platform.unwrap_or(host_platform());
    let before = dry_run(&registry, registry.config(), platform_name, "dev");
    let after = dry_run(&registry, &config, platform_name, "dev");
    let dependents: Vec<&String> = before
        .states
        .iter()
        .filter(|(other, state)| {
            other.as_str() != id
                && **state == ExtensionState::Active
                && after.states.get(*other) != Some(&ExtensionState::Active)
        })
        .map(|(other, _)| other)
        .collect();
    write_atomic(&project.join(crate::manifest::MANIFEST_FILE), &edited)?;
    Ok(json!({
        "disabled": true,
        "restart_required": !found.manifest.code.is_empty(),
        "dependents_affected": dependents,
    }))
}

fn generated_text(crates: &[String], config: &str, manifests: &[String]) -> String {
    let manifests: String = manifests.iter().map(|m| format!("        {m:?},\n")).collect();
    format!(
        "// Generated by `regia plugin sync`; commit this file and do not edit it.\n\
         // The extension crates this game links, the project's [extensions] table and the manifests\n\
         // of the rest of the set, which a packaged game cannot read from disk (specs/57 § 3.2).\n\
         regia::register_extensions! {{\n    \
             config = {config:?},\n    \
             manifests = [\n{manifests}    ],\n    \
             crates = [{}],\n\
         }}\n",
        crates.join(", ")
    )
}

/// `plugin sync`: writes the generated list; `cargo_missing` names crates not in `Cargo.toml`.
///
/// # Errors
///
/// `Regia.toml` cannot be read, or the file cannot be written.
pub fn sync(project: &Path) -> Result<Value> {
    let registry = load(project)?;
    // A crate a profile filter keeps out of one of the two sets is still linked in that build.
    let sets =
        ["dev", "shipping"].map(|profile| dry_run(&registry, registry.config(), host_platform(), profile));
    let mut linked: Vec<(String, String)> = sets
        .iter()
        .flat_map(|set| linked_crates(&registry, set))
        .collect();
    linked.sort();
    linked.dedup();
    let linked_ids: Vec<&str> = linked.iter().map(|(id, _)| id.as_str()).collect();
    let mut crates: Vec<String> = linked.iter().map(|(_, name)| name.clone()).collect();
    crates.sort();
    crates.dedup();
    let mut ids: Vec<&str> = sets
        .iter()
        .flat_map(|set| set.order.iter().map(|e| e.id.as_str()))
        .filter(|id| !linked_ids.contains(id))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let mut manifests = Vec::new();
    for found in ids.into_iter().filter_map(|id| registry.discovered(id)) {
        // The exact bytes: the hash a cook records is the hash of the file.
        let path = Path::new(&found.path).join(MANIFEST_FILE);
        manifests.push(std::fs::read_to_string(&path).with_context(|| path.display().to_string())?);
    }
    let config = registry.config().to_project_toml()?;
    let path = project.join(GENERATED_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| parent.display().to_string())?;
    }
    write_atomic(&path, &generated_text(&crates, &config, &manifests))?;
    let dependencies = crate_table(project)
        .and_then(|table| table.get("dependencies").and_then(toml::Value::as_table).cloned())
        .unwrap_or_default();
    let missing: Vec<&String> = crates
        .iter()
        .filter(|name| {
            !dependencies.contains_key(*name) && !dependencies.contains_key(&name.replace('_', "-"))
        })
        .collect();
    Ok(json!({ "linked": crates, "cargo_missing": missing }))
}

fn run_cargo(command: &mut Command) -> Result<()> {
    let status = command.status().context("cannot run cargo")?;
    if status.success() {
        Ok(())
    } else {
        bail!("cargo failed ({status})")
    }
}

fn rustc_version() -> Result<String> {
    let output = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned()))
        .arg("--version")
        .output()
        .context("cannot run rustc")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `plugin build`: links the engine dylib, reads its fingerprint, then builds each source entry
/// as a dylib carrying the stamp, into `<ext>/bin/<target-triple>/`.
///
/// # Errors
///
/// Unknown id, no crate to build, Android, or a failed cargo run.
pub fn build(project: &Path, id: &str, release: bool, target_dir: Option<&Path>) -> Result<Value> {
    if cfg!(target_os = "android") {
        bail!("not_supported: dynamic extensions do not exist on Android");
    }
    let registry = load(project)?;
    let found = registry
        .discovered(id)
        .with_context(|| format!("no extension `{id}` was discovered"))?;
    let dir = PathBuf::from(&found.path);
    let target = target_dir.map_or_else(|| project.join("target"), Path::to_path_buf);
    let profile = if release { "release" } else { "debug" };
    let abi_hash = stamp::abi_hash_for(&rustc_version()?, BUILD_TARGET, profile);
    let out_dir = dir.join("bin").join(BUILD_TARGET);
    std::fs::create_dir_all(&out_dir).with_context(|| out_dir.display().to_string())?;
    let mut libraries = Vec::new();
    for entry in &found.manifest.code {
        let (Some(name), Some(crate_root)) = (entry.crate_name.as_deref(), crate_dir(&dir, entry)) else {
            continue;
        };
        let manifest_path = crate_root.join("Cargo.toml");
        let flags: &[&str] = if release { &["--release"] } else { &[] };
        // The fingerprint reads the engine dylib, so it must exist first.
        run_cargo(
            Command::new("cargo")
                .arg("build")
                .arg("--manifest-path")
                .arg(&manifest_path)
                .args(["-p", "regia_dylib"])
                .args(flags)
                .arg("--target-dir")
                .arg(&target),
        )?;
        let fingerprint = stamp::engine_fingerprint(&target, profile);
        run_cargo(
            Command::new("cargo")
                .arg("rustc")
                .arg("--manifest-path")
                .arg(&manifest_path)
                .args([
                    "-p",
                    name,
                    "--lib",
                    "--crate-type",
                    "dylib",
                    "--features",
                    "dynamic-extension",
                ])
                .args(flags)
                .arg("--target-dir")
                .arg(&target)
                .env("REGIA_ABI_HASH", abi_hash.to_string())
                .env("REGIA_ENGINE_FINGERPRINT", fingerprint.to_string())
                .args(["--", "-C", "prefer-dynamic"]),
        )?;
        let prefix = if cfg!(windows) { "" } else { "lib" };
        let file = format!("{prefix}{}.{}", name.replace('-', "_"), dylib_extension());
        let built = target.join(profile).join(&file);
        let dest = out_dir.join(&file);
        std::fs::copy(&built, &dest).with_context(|| format!("{} to {}", built.display(), dest.display()))?;
        libraries.push(dest.display().to_string());
    }
    if libraries.is_empty() {
        bail!("`{id}` has no source crate to build (no [[code]] crate with a Cargo.toml)");
    }
    Ok(json!({ "libraries": libraries, "abi_hash": abi_hash, "profile": profile }))
}

/// Writes `extensions.ron` into `pak_dir` for `platform` when the project resolves any extension
/// (specs/57 § 4.4); removes a stale one when it resolves none.
///
/// # Errors
///
/// `Regia.toml` cannot be read, or the file cannot be written.
pub fn write_cooked_record(project: &Path, platform: &str, pak_dir: &Path) -> Result<()> {
    let registry = load(project)?;
    let set = dry_run(&registry, registry.config(), platform, "shipping");
    let path = pak_dir.join(COOKED_FILE);
    if set.order.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let text = CookedExtensions::of(&set, &registry.inputs().discovery.manifests).to_ron()?;
    std::fs::create_dir_all(pak_dir).with_context(|| pak_dir.display().to_string())?;
    write_atomic(&path, &text)
}

/// Cooks what the project references of its enabled extensions, and the extension record, into
/// the cooked cache. Runs after `cook_project` and before the paks are written (spec § 4.4).
///
/// # Errors
///
/// `Regia.toml` cannot be read, the cache cannot be written, or a referenced file failed to cook.
pub fn cook_extensions(project: &Path, platform: &str, incremental: bool) -> Result<()> {
    let registry = load(project)?;
    let set = dry_run(&registry, registry.config(), platform, "shipping");
    let mut roots = Vec::new();
    for resolved in &set.order {
        if let Some(found) = registry.discovered(&resolved.id) {
            for root in &found.manifest.content {
                let root = content_root(Path::new(&found.path), root)
                    .map_err(|error| anyhow::anyhow!("path_escape: `{}`: {error}", resolved.id))?;
                roots.push(ExtensionRoot {
                    id: resolved.id.clone(),
                    root,
                });
            }
        }
    }
    let record = if set.order.is_empty() {
        None
    } else {
        let text = CookedExtensions::of(&set, &registry.inputs().discovery.manifests).to_ron()?;
        Some((COOKED_ASSET_PATH.to_owned(), text))
    };
    let report = cook_extension_content(&ExtensionCookOptions {
        project_root: project.to_path_buf(),
        platform: platform.to_owned(),
        incremental,
        roots,
        record,
    })?;
    for path in &report.missing {
        eprintln!("warning: `{path}` is referenced, but no enabled extension provides it");
    }
    if let Some((path, error)) = report.errors.first() {
        bail!(
            "{} extension file(s) failed to cook; first: {path}: {error}",
            report.errors.len()
        );
    }
    Ok(())
}

/// Whether the paks of `platform` still hold the current extension set and content: the record
/// next to them matches, and every extension file a cook recorded still has that content.
#[must_use]
pub fn cooked_extensions_fresh(project: &Path, platform: &str, pak_dir: &Path) -> bool {
    let Ok(registry) = load(project) else {
        return false;
    };
    let set = dry_run(&registry, registry.config(), platform, "shipping");
    let on_disk = std::fs::read_to_string(pak_dir.join(COOKED_FILE)).ok();
    let current = if set.order.is_empty() {
        None
    } else {
        CookedExtensions::of(&set, &registry.inputs().discovery.manifests)
            .to_ron()
            .ok()
    };
    if on_disk != current {
        return false;
    }
    let Ok(db) = regia_asset_pipeline::AssetDatabase::open(project) else {
        return false;
    };
    db.entries
        .iter()
        .filter(|(path, _)| path.starts_with("ext://") && path.as_str() != COOKED_ASSET_PATH)
        .all(|(path, row)| {
            let Some((id, rest)) = path.trim_start_matches("ext://").split_once('/') else {
                return false;
            };
            registry.discovered(id).is_some_and(|found| {
                found.manifest.content.iter().any(|root| {
                    std::fs::read(Path::new(&found.path).join(root).join(rest))
                        .is_ok_and(|bytes| *blake3::hash(&bytes).as_bytes() == row.source_hash)
                })
            })
        })
}

/// `plugin tool`: the checks and the run `plugin.tool_run` does, for a shell.
///
/// # Errors
///
/// Unknown id or tool, a capability not granted, a timeout, too much output.
pub fn run_tool(project: &Path, id: &str, name: &str, args: Vec<String>) -> Result<Value> {
    let registry = load(project)?;
    let prepared = regia_agent::plugins::prepare_tool(&registry, id, name, args, Vec::new())
        .map_err(|error| anyhow::anyhow!(error.message))?;
    let output = regia_framework::extension::tool::run_tool(&prepared)?;
    Ok(regia_agent::plugins::tool_json(&output))
}

fn engine_dependency(project: &Path) -> String {
    let dependency = crate_table(project).and_then(|table| table.get("dependencies")?.get("regia").cloned());
    if let Some(toml::Value::Table(mut table)) = dependency {
        // The new crate sits two directories below the project.
        if let Some(path) = table.get("path").and_then(toml::Value::as_str).map(str::to_owned)
            && Path::new(&path).is_relative()
        {
            table.insert("path".into(), toml::Value::String(format!("../../{path}")));
        }
        let fields: Vec<String> = table
            .iter()
            .map(|(key, value)| format!("{key} = {value}"))
            .collect();
        return format!("{{ {} }}", fields.join(", "));
    }
    format!("\"{}\"", stamp::ENGINE_VERSION)
}

fn dylib_dependency(regia: &str) -> String {
    if regia.contains("path") {
        // The engine's path dependency names `crates/regia`; the dylib crate sits beside it.
        format!(
            "{}, optional = true }}",
            regia
                .replace("crates/regia\"", "crates/regia_dylib\"")
                .trim_end_matches(" }")
        )
    } else {
        format!("{{ version = {regia}, optional = true }}")
    }
}

fn manifest_template(id: &str, content: bool) -> String {
    let mut text = format!(
        "format_version = 1\nid = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\npublisher = \"\"\nlicence = \"LicenseRef-Proprietary\"\ndescription = \"\"\n"
    );
    text.push_str(if content {
        "\n[[content]]\nroot = \"content\"\n"
    } else {
        "capabilities = [\"systems\"]\n\n[[code]]\n"
    });
    if !content {
        text.push_str(&format!("crate = \"{id}\"\naudience = [\"game\"]\n"));
    }
    text
}

fn cargo_template(id: &str, regia: &str) -> String {
    format!(
        "[package]\nname = \"{id}\"\nversion = \"0.1.0\"\nedition = \"2024\"\ninclude = [\"src/**\", \"Cargo.toml\", \"{MANIFEST_FILE}\"]\n\n\
         [package.metadata.regia]\nid = \"{id}\"\n\n[features]\n\
         # `regia plugin build` turns this on: the stamp and entry symbols (specs/57 § 2.8).\n\
         dynamic-extension = [\"dep:regia_dylib\"]\n\n[dependencies]\nregia = {regia}\nregia_dylib = {}\n",
        dylib_dependency(regia)
    )
}

fn source_template(id: &str) -> String {
    let ty: String = id
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect();
    format!(
        "//! The `{id}` extension.\n\nuse regia::extension::prelude::*;\n\n/// Registers this extension's content.\n#[derive(Default)]\npub struct {ty};\n\n\
         impl RegiaExtension for {ty} {{\n    fn id(&self) -> &str {{\n        \"{id}\"\n    }}\n\n    \
         fn build(&self, _cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {{\n        Ok(())\n    }}\n}}\n\n\
         regia::extension!({ty});\n"
    )
}

/// `plugin new`: scaffolds `extensions/<id>` and returns the directory.
///
/// # Errors
///
/// A bad id, or the directory exists.
pub fn new_extension(project: &Path, id: &str, content: bool) -> Result<PathBuf> {
    if !regia_framework::extension::manifest::valid_id(id) {
        bail!("`{id}` is not an extension id: lowercase, 2-32 of a-z, 0-9 and _, starting with a letter");
    }
    if reserved_namespaces().iter().any(|reserved| reserved == id) {
        bail!("reserved_id: `{id}` is an engine namespace");
    }
    let dir = project.join("extensions").join(id);
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    std::fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
    std::fs::write(dir.join(MANIFEST_FILE), manifest_template(id, content))?;
    if content {
        std::fs::create_dir_all(dir.join("content"))?;
    } else {
        std::fs::create_dir_all(dir.join("src"))?;
        std::fs::write(
            dir.join("Cargo.toml"),
            cargo_template(id, &engine_dependency(project)),
        )?;
        std::fs::write(dir.join("src/lib.rs"), source_template(id))?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::extension::{ExtensionPlugin, LinkedExtensions};

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, text).expect("write");
    }

    struct Project(PathBuf);

    impl Project {
        fn new(tag: &str) -> Self {
            let dir = regia_test_utils::unique_temp_dir(tag);
            write(&dir.join("Regia.toml"), "# mine\n[project]\nname = \"p\"\n");
            Self(dir)
        }

        fn manifest(&self, id: &str, body: &str) {
            write(
                &self.0.join("extensions").join(id).join(MANIFEST_FILE),
                &format!("format_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\n{body}"),
            );
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const CODE: &str = "[[code]]\ncrate = \"thing\"\naudience = [\"game\"]\n";

    fn codes(report: &Value) -> Vec<&str> {
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .filter_map(|d| d["code"].as_str())
            .collect()
    }

    #[test]
    fn new_enable_check_disable_round_trip() {
        let p = Project::new("cli-plugin-round");
        let dir = new_extension(&p.0, "chatter", false).expect("new");
        assert!(dir.join("src/lib.rs").is_file());
        assert!(new_extension(&p.0, "chatter", false).is_err(), "exists");
        assert!(new_extension(&p.0, "scene", false).is_err(), "reserved");
        // `dialogue.*` agent methods (spec 56) made `dialogue` an engine namespace too.
        assert!(new_extension(&p.0, "dialogue", false).is_err(), "reserved");
        assert!(new_extension(&p.0, "Bad-Id", false).is_err());
        let list = list_json(&p.0, None).expect("list");
        assert_eq!(list["extensions"][0]["state"], "available");

        let answer = enable(&p.0, "chatter", &[], false, None).expect("enable");
        assert_eq!(answer["restart_required"], true);
        let text = std::fs::read_to_string(p.0.join("Regia.toml")).expect("read");
        assert!(text.starts_with("# mine\n"), "{text}");
        assert_eq!(
            list_json(&p.0, Some("active")).expect("list")["extensions"][0]["id"],
            "chatter"
        );

        let report = check(&p.0, None, "dev").expect("check");
        assert_eq!(report["ok"], true, "{report}");
        assert_eq!(report["order"][0]["id"], "chatter");

        let answer = disable(&p.0, "chatter", None).expect("disable");
        assert_eq!(answer["disabled"], true);
        assert_eq!(
            list_json(&p.0, Some("disabled")).expect("list")["extensions"][0]["id"],
            "chatter"
        );
        assert!(list_json(&p.0, Some("odd")).is_err());
    }

    #[test]
    fn a_content_extension_needs_no_crate_or_restart() {
        let p = Project::new("cli-plugin-content");
        let dir = new_extension(&p.0, "art_pack", true).expect("new");
        assert!(dir.join("content").is_dir());
        assert!(!dir.join("Cargo.toml").exists());
        let answer = enable(&p.0, "art_pack", &[], false, None).expect("enable");
        assert_eq!(answer["restart_required"], false);
    }

    #[test]
    fn enable_refuses_an_ungranted_capability() {
        let p = Project::new("cli-plugin-grant");
        p.manifest(
            "tooly",
            &format!("capabilities = [\"process\", \"systems\"]\n{CODE}"),
        );
        let refused = enable(&p.0, "tooly", &[], false, None).expect_err("needs a grant");
        assert!(
            refused.to_string().contains("capability_not_granted"),
            "{refused}"
        );
        assert!(enable(&p.0, "tooly", &["root".to_owned()], false, None).is_err());
        enable(&p.0, "tooly", &[], true, None).expect("--grant-all");
        let config = config(&p.0).expect("config");
        assert_eq!(config.grant_map()["tooly"], ["process"], "systems stays implicit");
    }

    #[test]
    fn check_reports_platform_and_exit_state() {
        let p = Project::new("cli-plugin-android");
        p.manifest(
            "prebuilt",
            "[[code]]\nlibrary = \"bin/libprebuilt.so\"\naudience = [\"game\"]\n",
        );
        write(
            &p.0.join("Regia.toml"),
            "[extensions]\nenabled = [\"prebuilt\"]\n",
        );
        let report = check(&p.0, Some("android"), "dev").expect("check");
        assert_eq!(report["ok"], false, "{report}");
        assert!(codes(&report).contains(&"not_supported"), "{report}");
        assert!(check(&p.0, Some("dos"), "dev").is_err());
        assert!(check(&p.0, None, "odd").is_err());
    }

    #[test]
    fn a_runtime_entry_may_not_link_the_editor() {
        let p = Project::new("cli-plugin-layer");
        p.manifest("mixed", "[[code]]\ncrate = \"mixed\"\naudience = [\"game\"]\n");
        write(
            &p.0.join("extensions/mixed/Cargo.toml"),
            "[package]\nname = \"mixed\"\ninclude = [\"src/**\"]\n\n[dependencies]\nregia_editor = \"1\"\n",
        );
        write(&p.0.join("Regia.toml"), "[extensions]\nenabled = [\"mixed\"]\n");
        let report = check(&p.0, None, "dev").expect("check");
        assert_eq!(report["ok"], false, "{report}");
        let found = codes(&report);
        assert!(found.contains(&"layer_violation"), "{report}");
        assert!(found.contains(&"package_include"), "{report}");
    }

    #[test]
    fn sync_writes_the_list_and_check_cross_checks_it() {
        let p = Project::new("cli-plugin-sync");
        p.manifest("zebra", "[[code]]\ncrate = \"zebra-rt\"\naudience = [\"game\"]\n");
        p.manifest(
            "apple",
            "[[code]]\ncrate = \"apple_rt\"\naudience = [\"game\", \"server\"]\n",
        );
        p.manifest(
            "tooling",
            "[[code]]\ncrate = \"tooling_ed\"\naudience = [\"editor\"]\n",
        );
        write(
            &p.0.join("Regia.toml"),
            "[extensions]\nenabled = [\"zebra\", \"apple\", \"tooling\"]\n",
        );
        write(
            &p.0.join("Cargo.toml"),
            "[package]\nname = \"g\"\n\n[dependencies]\napple_rt = \"1\"\n",
        );
        let answer = sync(&p.0).expect("sync");
        assert_eq!(
            answer["linked"],
            json!(["apple_rt", "zebra_rt"]),
            "editor entries are not linked"
        );
        assert_eq!(answer["cargo_missing"], json!(["zebra_rt"]));
        assert_eq!(
            generated_crates(&p.0).expect("generated"),
            ["apple_rt", "zebra_rt"]
        );
        assert_eq!(check(&p.0, None, "dev").expect("check")["ok"], true);

        // The list goes stale in both directions.
        write(
            &p.0.join(GENERATED_FILE),
            "regia::register_extensions![apple_rt, gone];\n",
        );
        let report = check(&p.0, None, "dev").expect("check");
        let found = codes(&report);
        assert!(found.contains(&"manifest_not_linked"), "{report}");
        assert!(found.contains(&"linked_not_enabled"), "{report}");
        assert_eq!(report["ok"], false);
    }

    #[cfg(unix)]
    #[test]
    fn tools_need_the_process_grant_and_run_bounded() {
        let p = Project::new("cli-plugin-tool");
        p.manifest(
            "lint",
            "capabilities = [\"process\"]\n\n[[tool]]\nname = \"say\"\ncommand = \"bin/say\"\ntimeout_seconds = 30\n",
        );
        let script = p.0.join("extensions/lint/bin/say");
        // Written by a child process (ETXTBSY under parallel tests, ADR-5959).
        std::fs::create_dir_all(script.parent().expect("bin dir")).expect("bin dir");
        regia_test_utils::process::write_executable_script(&script, "#!/bin/sh\necho \"said $1\"\n")
            .expect("tool");
        let refused = run_tool(&p.0, "lint", "say", vec!["hi".into()]).expect_err("not enabled");
        assert!(
            refused.to_string().contains("capability_not_granted"),
            "{refused}"
        );
        enable(&p.0, "lint", &[], true, None).expect("enable");
        let answer = run_tool(&p.0, "lint", "say", vec!["hi".into()]).expect("run");
        assert_eq!(answer["status"], 0);
        assert_eq!(answer["stdout"], "said hi\n");
        assert!(run_tool(&p.0, "lint", "nothing", Vec::new()).is_err());
    }

    #[test]
    fn the_cooked_record_follows_the_resolved_set() {
        let p = Project::new("cli-plugin-cooked");
        p.manifest("art", "[[content]]\nroot = \"content\"\n");
        std::fs::create_dir_all(p.0.join("extensions/art/content")).expect("content");
        let paks = p.0.join("build/paks/linux");
        write_cooked_record(&p.0, "linux", &paks).expect("nothing to record");
        assert!(!paks.join(COOKED_FILE).exists());
        enable(&p.0, "art", &[], false, None).expect("enable");
        write_cooked_record(&p.0, "linux", &paks).expect("record");
        let text = std::fs::read_to_string(paks.join(COOKED_FILE)).expect("read");
        let record = CookedExtensions::parse(&text).expect("parse");
        assert_eq!(record.extensions[0].id, "art");
        assert_eq!(record.extensions[0].version, "1.0.0");
        disable(&p.0, "art", None).expect("disable");
        write_cooked_record(&p.0, "linux", &paks).expect("stale record removed");
        assert!(!paks.join(COOKED_FILE).exists());
    }

    /// The cook, the pack and the record, as `regia cook` runs them.
    fn cook_and_pack(root: &Path) -> PathBuf {
        let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
            project_root: root.to_path_buf(),
            platform: "linux".into(),
            incremental: true,
        })
        .expect("cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        cook_extensions(root, "linux", true).expect("extensions");
        let cache = regia_asset::cooked_cache_dir(root, "linux");
        let paks = root.join("build/paks/linux");
        crate::commands::content::pack_project_chunks(root, &cache, &paks, "linux", false, false)
            .expect("pack");
        write_cooked_record(root, "linux", &paks).expect("record");
        paks
    }

    struct DlgLoader;
    impl regia_framework::extension::AssetLoaderDesc for DlgLoader {
        fn file_extensions(&self) -> &[&str] {
            &["dlg"]
        }
        fn type_name(&self) -> &str {
            "fixture::Dialogue"
        }
        fn load(&self, bytes: &[u8], _path: &str) -> Result<Box<dyn regia_app::prelude::Reflect>, String> {
            Ok(Box::new(String::from_utf8_lossy(bytes).into_owned()))
        }
    }

    #[derive(Default)]
    struct Dialogue;
    impl regia_framework::extension::RegiaExtension for Dialogue {
        fn id(&self) -> &str {
            "chatter"
        }
        fn build(
            &self,
            cx: &mut regia_framework::extension::ExtensionContext<'_>,
        ) -> Result<(), regia_framework::extension::ExtensionError> {
            cx.register_asset_loader(Box::new(DlgLoader))
        }
    }

    const DIALOGUE: &str = "format_version = 1\nid = \"chatter\"\nversion = \"1.0.0\"\ncapabilities = [\"asset_loader\"]\n\n[[code]]\ncrate = \"chatter\"\naudience = [\"game\"]\n\n[[content]]\nroot = \"content\"\n";

    fn packaged_project(tag: &str) -> Project {
        let p = Project::new(tag);
        write(&p.0.join("extensions/chatter").join(MANIFEST_FILE), DIALOGUE);
        write(&p.0.join("extensions/chatter/content/hello.dlg"), "HELLO");
        write(&p.0.join("extensions/chatter/content/unused.dlg"), "NOBODY");
        let mut material = regia_asset::MaterialAssetRon::default_standard();
        material.name = Some("base".to_owned());
        let text = material.to_ron_string().expect("material");
        write(&p.0.join("assets/base.material.ron"), &text);
        // The project's file shadows the extension's.
        write(&p.0.join("assets/shadow.material.ron"), &text);
        write(&p.0.join("extensions/chatter/content/shadow.material.ron"), &text);
        write(
            &p.0.join("src/game.rs"),
            "const A: &str = \"ext://chatter/hello.dlg\";\n\
             const B: &str = \"ext://chatter/shadow.material.ron\";\n",
        );
        write(
            &p.0.join("Regia.toml"),
            "[extensions]\nenabled = [\"chatter\"]\n[extensions.grants]\nchatter = [\"asset_loader\"]\n",
        );
        p
    }

    /// A packaged game: the paks in a content directory, no project, no cooked cache.
    fn packaged_server(paks: &Path) -> (PathBuf, regia_asset::AssetServer) {
        let root = regia_test_utils::unique_temp_dir("cli-plugin-player");
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: Some(paks.to_path_buf()),
        })
        .expect("server");
        (root, server)
    }

    fn player(server: regia_asset::AssetServer, set: LinkedExtensions) -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_app::MinimalPlugins);
        app.insert_resource(server);
        app.add_plugins(ExtensionPlugin::for_game(None, set));
        app.update();
        app
    }

    fn linked_chatter(manifest: &'static str) -> LinkedExtensions {
        LinkedExtensions {
            extensions: vec![std::sync::Arc::new(Dialogue)],
            manifests: vec![manifest],
            config: "[extensions]\nenabled = [\"chatter\"]\n[extensions.grants]\nchatter = [\"asset_loader\"]\n",
        }
    }

    #[test]
    fn a_packaged_game_reads_extension_content_from_the_pak() {
        let p = packaged_project("cli-plugin-pak");
        let paks = cook_and_pack(&p.0);
        let (root, server) = packaged_server(&paks);
        assert!(
            server.id_for_path("ext://chatter/unused.dlg").is_none(),
            "unreferenced content stays out of the pak"
        );
        assert!(server.id_for_path("assets/shadow.material.ron").is_some());
        assert_eq!(
            server.id_for_path("ext://chatter/shadow.material.ron"),
            server.id_for_path("assets/shadow.material.ron"),
            "a shadowed path names the project's asset in the pak too"
        );
        let mut app = player(server, linked_chatter(DIALOGUE));
        let world = app.world_mut();
        let registry = world.remove_resource::<ExtensionRegistry>().expect("registry");
        assert!(registry.diagnostics().is_empty(), "{:?}", registry.diagnostics());
        let mut server = world.resource_mut::<regia_asset::AssetServer>();
        let value = registry
            .load_cooked_content(&mut server, "ext://chatter/hello.dlg")
            .expect("load");
        assert_eq!(value.downcast_ref::<String>().map(String::as_str), Some("HELLO"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-2844: the asset server's own typed load reaches the extension's loader through the
    /// hook the extension plugin installs, with the same bytes `load_cooked_content` decodes.
    #[test]
    fn the_asset_servers_content_load_reaches_the_extension_loader() {
        let p = packaged_project("cli-plugin-hook");
        let paks = cook_and_pack(&p.0);
        let (root, server) = packaged_server(&paks);
        let mut app = player(server, linked_chatter(DIALOGUE));
        let mut server = app.world_mut().resource_mut::<regia_asset::AssetServer>();
        assert!(server.has_content_hook());
        assert!(server.has_content_loader("ext://chatter/hello.dlg"));
        let value = server
            .load_content::<Box<dyn regia_app::prelude::Reflect>>("ext://chatter/hello.dlg")
            .expect("load");
        assert_eq!(value.downcast_ref::<String>().map(String::as_str), Some("HELLO"));
        // A kind no extension claims, a path that is not cooked, and the wrong requested type.
        assert!(!server.has_content_loader("assets/shadow.material.ron"));
        assert!(server.load_content_any("assets/shadow.material.ron").is_err());
        assert!(server.load_content_any("ext://chatter/missing.dlg").is_err());
        assert!(server.load_content::<u32>("ext://chatter/hello.dlg").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_binary_that_disagrees_with_the_pak_is_reported() {
        let p = packaged_project("cli-plugin-pak-mismatch");
        let paks = cook_and_pack(&p.0);
        let (root, server) = packaged_server(&paks);
        // The manifest this binary was built with differs from the one the content was cooked with.
        let edited: &'static str = Box::leak(format!("{DIALOGUE}\n# edited\n").into_boxed_str());
        let app = player(server, linked_chatter(edited));
        let registry = app.world().resource::<ExtensionRegistry>();
        let codes: Vec<&str> = registry.diagnostics().iter().map(|d| d.code).collect();
        assert!(codes.contains(&"extension_set_mismatch"), "{codes:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_content_root_that_leads_out_stops_the_cook() {
        let p = packaged_project("cli-plugin-escape");
        let outside = regia_test_utils::unique_temp_dir("cli-plugin-outside");
        write(&outside.join("hello.dlg"), "SECRET");
        let content = p.0.join("extensions/chatter/content");
        std::fs::remove_dir_all(&content).expect("remove");
        std::os::unix::fs::symlink(&outside, &content).expect("link");
        let error = cook_extensions(&p.0, "linux", true).expect_err("refused");
        assert!(error.to_string().contains("path_escape"), "{error}");
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn extension_changes_make_the_paks_stale() {
        let p = packaged_project("cli-plugin-fresh");
        let paks = cook_and_pack(&p.0);
        assert!(cooked_extensions_fresh(&p.0, "linux", &paks));
        write(&p.0.join("extensions/chatter/content/hello.dlg"), "CHANGED");
        assert!(
            !cooked_extensions_fresh(&p.0, "linux", &paks),
            "the content changed"
        );
        cook_and_pack(&p.0);
        assert!(cooked_extensions_fresh(&p.0, "linux", &paks));
        disable(&p.0, "chatter", None).expect("disable");
        assert!(!cooked_extensions_fresh(&p.0, "linux", &paks), "the set changed");
    }

    #[test]
    fn sync_embeds_what_a_packaged_game_cannot_read_from_disk() {
        let p = Project::new("cli-plugin-sync-embed");
        p.manifest("rt", "[[code]]\ncrate = \"rt_crate\"\naudience = [\"game\"]\n");
        p.manifest(
            "tooling",
            "[[code]]\ncrate = \"tooling_ed\"\naudience = [\"editor\"]\n",
        );
        p.manifest("art", "[[content]]\nroot = \"content\"\n");
        write(
            &p.0.join("Regia.toml"),
            "[extensions]\nenabled = [\"rt\", \"tooling\", \"art\"]\n",
        );
        std::fs::create_dir_all(p.0.join("extensions/art/content")).expect("content");
        sync(&p.0).expect("sync");
        let text = std::fs::read_to_string(p.0.join(GENERATED_FILE)).expect("read");
        assert!(text.contains("crates = [rt_crate]"), "{text}");
        assert!(text.contains("id = \\\"art\\\""), "{text}");
        assert!(text.contains("id = \\\"tooling\\\""), "{text}");
        assert!(
            !text.contains("id = \\\"rt\\\""),
            "the crate embeds its own: {text}"
        );
        assert!(text.contains("config = \""), "{text}");
        assert_eq!(generated_crates(&p.0).expect("generated"), ["rt_crate"]);
    }

    #[test]
    fn templates_parse_and_keep_a_relative_engine_path() {
        let p = Project::new("cli-plugin-template");
        write(
            &p.0.join("Cargo.toml"),
            "[package]\nname = \"g\"\n\n[dependencies]\nregia = { path = \"../engine/crates/regia\" }\n",
        );
        let dir = new_extension(&p.0, "with_path", false).expect("new");
        let table = crate_table(&dir).expect("Cargo.toml parses");
        let deps = table["dependencies"].as_table().expect("deps");
        assert_eq!(
            deps["regia"]["path"].as_str(),
            Some("../../../engine/crates/regia")
        );
        assert_eq!(
            deps["regia_dylib"]["path"].as_str(),
            Some("../../../engine/crates/regia_dylib")
        );
        assert_eq!(deps["regia_dylib"]["optional"].as_bool(), Some(true));
        let found = discover::read_extension_dir(&dir).expect("the manifest template is valid");
        assert_eq!(found.manifest.id, "with_path");
        let plain = Project::new("cli-plugin-template-plain");
        let dir = new_extension(&plain.0, "versioned", false).expect("new");
        crate_table(&dir).expect("Cargo.toml parses");
    }
}
