// SPDX-License-Identifier: MPL-2.0
//! Tool manifests (ADR-1523): one `<id>.tool.toml` per out-of-process editor tool, saying how to
//! launch it, what its scoped token may call, which methods it answers and what its panel shows.
//!
//! Manifests are found in the project (`<project>/.regia/tools/`), the user directory
//! (`<user dir>/tools/`) and every directory of `REGIA_TOOL_PATH`, in that order; the first
//! manifest of an id wins. A manifest that does not parse or validate is listed with its error
//! and never stops the editor.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_agent::schema::Permission;
use serde::Deserialize;
use serde_json::{Value, json};

/// The manifest format this editor reads.
pub const MANIFEST_FORMAT: u32 = 1;

/// The file name suffix of a manifest: `<id>.tool.toml`.
pub const MANIFEST_SUFFIX: &str = ".tool.toml";

/// Extra manifest directories, separated like `PATH`.
pub const TOOL_PATH_ENV: &str = "REGIA_TOOL_PATH";

/// Environment variables the launcher sets; a manifest's `[launch.env]` may not set them.
pub const RESERVED_ENV_PREFIX: &str = "REGIA_TOOL_";

/// Most entries in each manifest list (`args`, `methods`, `provides`, `params`, `env`).
pub const MAX_LIST: usize = 64;

/// Longest id or method name, in bytes.
pub const MAX_NAME_BYTES: usize = 64;

/// How to start the tool.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLaunch {
    /// The program: an absolute path, a path relative to the manifest's directory (it has a
    /// separator), or a bare name looked up on `PATH`.
    pub program: String,
    /// Its arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// The working directory, relative to the manifest's directory (default: that directory).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables (never `REGIA_TOOL_*` or `REGIA_PROJECT_DIR`).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Launch when the editor opens a project. Honored for manifests outside the project only.
    #[serde(default)]
    pub autostart: bool,
}

/// What the tool's token may call (the project's `.regia/agent.toml` still applies on top).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolScopeSpec {
    /// Permission names: `read`, `edit_scene`, `play`, `assets` (never `tools`).
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Engine method patterns: exact names, prefixes ending in `*`, or `*`.
    #[serde(default)]
    pub methods: Vec<String>,
}

/// One parameter of a provided method (the `agent.schema` row shape).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidedParam {
    /// Parameter name.
    pub name: String,
    /// JSON type: `object`, `array`, `string`, `number`, `boolean` or `any` (default).
    #[serde(default = "any_kind")]
    pub kind: String,
    /// Whether a call must pass it.
    #[serde(default)]
    pub required: bool,
    /// What it does.
    #[serde(default)]
    pub description: String,
}

fn any_kind() -> String {
    "any".to_string()
}

fn read_permission() -> String {
    "read".to_string()
}

/// A method the tool answers, called through `editor.tool_call`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidedMethod {
    /// Method name (`[a-z0-9_.]`, e.g. `pick.list`).
    pub name: String,
    /// One line on what it does.
    #[serde(default)]
    pub description: String,
    /// The permission a caller needs: `read` (default), `edit_scene`, `play` or `assets`.
    #[serde(default = "read_permission")]
    pub permission: String,
    /// Its parameters.
    #[serde(default)]
    pub params: Vec<ProvidedParam>,
}

impl ProvidedMethod {
    /// The declared permission (validated manifests always have one).
    #[must_use]
    pub fn permission(&self) -> Permission {
        Permission::from_name(&self.permission).unwrap_or(Permission::Read)
    }

    /// The method as `editor.tool_methods` lists it.
    #[must_use]
    pub fn to_json(&self, tool: &str) -> Value {
        json!({
            "tool": tool,
            "name": self.name,
            "qualified": qualified_name(tool, &self.name),
            "description": self.description,
            "permission": self.permission,
            "params": self.params.iter().map(|p| json!({
                "name": p.name,
                "kind": p.kind,
                "required": p.required,
                "description": p.description,
            })).collect::<Vec<_>>(),
        })
    }
}

/// What the Tools panel shows for the tool besides its own view.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPanelSpec {
    /// The panel heading (default: the tool's name).
    #[serde(default)]
    pub title: Option<String>,
    /// A page the panel offers to open in the browser (`http` or `https` only).
    #[serde(default)]
    pub url: Option<String>,
}

/// One parsed and validated `<id>.tool.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolManifest {
    /// The format version ([`MANIFEST_FORMAT`]).
    pub format: u32,
    /// Unique id (`[a-z0-9_-]`, 1-64 bytes); the file is `<id>.tool.toml`.
    pub id: String,
    /// Display name (default: the id).
    #[serde(default)]
    pub name: String,
    /// The tool's own version, shown in the panel.
    #[serde(default)]
    pub version: String,
    /// One line on what it is.
    #[serde(default)]
    pub description: String,
    /// How to start it.
    pub launch: ToolLaunch,
    /// What its token may call.
    #[serde(default)]
    pub scope: ToolScopeSpec,
    /// The methods it answers.
    #[serde(default)]
    pub provides: Vec<ProvidedMethod>,
    /// Panel settings.
    #[serde(default)]
    pub panel: ToolPanelSpec,
}

impl ToolManifest {
    /// Parses and validates manifest text.
    ///
    /// # Errors
    ///
    /// The first problem found, as a sentence naming the key.
    pub fn parse(text: &str) -> Result<Self, String> {
        // Read the format first, so a newer manifest says so instead of failing on a new key.
        let value: toml::Value = toml::from_str(text).map_err(|e| format!("TOML parse error: {e}"))?;
        match value.get("format").and_then(toml::Value::as_integer) {
            Some(format) if format == i64::from(MANIFEST_FORMAT) => {}
            Some(format) if format > i64::from(MANIFEST_FORMAT) => {
                return Err(format!(
                    "format {format} is newer than this editor reads ({MANIFEST_FORMAT}); update the editor"
                ));
            }
            Some(format) => {
                return Err(format!(
                    "format {format} is not a manifest format (use {MANIFEST_FORMAT})"
                ));
            }
            None => return Err(format!("`format = {MANIFEST_FORMAT}` is missing")),
        }
        let mut manifest: Self = toml::from_str(text).map_err(|e| format!("invalid manifest: {e}"))?;
        manifest.validate()?;
        if manifest.name.is_empty() {
            manifest.name = manifest.id.clone();
        }
        Ok(manifest)
    }

    /// Reads, parses and validates `path`; the file name must be `<id>.tool.toml`.
    ///
    /// # Errors
    ///
    /// The read error or the first problem found.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let manifest = Self::parse(&text)?;
        let file = path.file_name().and_then(|f| f.to_str()).unwrap_or_default();
        let expected = format!("{}{MANIFEST_SUFFIX}", manifest.id);
        if file != expected {
            return Err(format!(
                "the file of tool `{}` must be named {expected}, not {file}",
                manifest.id
            ));
        }
        Ok(manifest)
    }

    /// The panel heading.
    #[must_use]
    pub fn title(&self) -> &str {
        self.panel.title.as_deref().unwrap_or(&self.name)
    }

    /// The declared method `name`.
    #[must_use]
    pub fn provided(&self, name: &str) -> Option<&ProvidedMethod> {
        self.provides.iter().find(|m| m.name == name)
    }

    /// The scope's permissions (validated manifests have only known names).
    #[must_use]
    pub fn scope_permissions(&self) -> Vec<Permission> {
        self.scope
            .permissions
            .iter()
            .filter_map(|name| Permission::from_name(name))
            .collect()
    }

    fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.id) {
            return Err(format!(
                "`id` \"{}\" must be 1-{MAX_NAME_BYTES} bytes of a-z, 0-9, `_` and `-`",
                self.id
            ));
        }
        if self.launch.program.trim().is_empty() {
            return Err("`launch.program` is empty".into());
        }
        check_len("launch.args", self.launch.args.len())?;
        check_len("launch.env", self.launch.env.len())?;
        check_len("scope.methods", self.scope.methods.len())?;
        check_len("scope.permissions", self.scope.permissions.len())?;
        check_len("provides", self.provides.len())?;
        for key in self.launch.env.keys() {
            if key.starts_with(RESERVED_ENV_PREFIX)
                || key == crate::tools::host::PROJECT_DIR_ENV
                || key.is_empty()
            {
                return Err(format!("`launch.env` may not set {key:?}: the launcher sets it"));
            }
        }
        for name in &self.scope.permissions {
            match Permission::from_name(name) {
                Some(Permission::Tools) => {
                    return Err(
                        "`scope.permissions` may not grant \"tools\": a tool cannot start tools".into(),
                    );
                }
                Some(Permission::Build) => {
                    return Err(
                        "`scope.permissions` may not grant \"build\": a tool cannot change which code loads"
                            .into(),
                    );
                }
                Some(_) => {}
                None => return Err(format!("`scope.permissions` names unknown permission {name:?}")),
            }
        }
        for pattern in &self.scope.methods {
            if !valid_pattern(pattern) {
                return Err(format!(
                    "`scope.methods` pattern {pattern:?} must be a method name, a prefix ending in `*`, or `*`"
                ));
            }
        }
        let mut names = BTreeSet::new();
        for method in &self.provides {
            if !valid_method_name(&method.name) {
                return Err(format!(
                    "`provides` name {:?} must be 1-{MAX_NAME_BYTES} bytes of a-z, 0-9, `_` and `.`, \
                     not starting or ending with `.`",
                    method.name
                ));
            }
            if !names.insert(method.name.as_str()) {
                return Err(format!("`provides` lists {} twice", method.name));
            }
            match Permission::from_name(&method.permission) {
                Some(Permission::Tools | Permission::Build) | None => {
                    return Err(format!(
                        "`provides` {} permission {:?} must be read, edit_scene, play or assets",
                        method.name, method.permission
                    ));
                }
                Some(_) => {}
            }
            check_len("provides.params", method.params.len())?;
            let mut params = BTreeSet::new();
            for param in &method.params {
                if param.name.is_empty() || !params.insert(param.name.as_str()) {
                    return Err(format!(
                        "`provides` {} has an empty or repeated parameter name",
                        method.name
                    ));
                }
                if !matches!(
                    param.kind.as_str(),
                    "object" | "array" | "string" | "number" | "boolean" | "any"
                ) {
                    return Err(format!(
                        "`provides` {} parameter {} kind {:?} must be object, array, string, number, boolean or any",
                        method.name, param.name, param.kind
                    ));
                }
            }
        }
        if let Some(url) = &self.panel.url
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            return Err(format!("`panel.url` {url:?} must start with http:// or https://"));
        }
        Ok(())
    }
}

fn check_len(key: &str, len: usize) -> Result<(), String> {
    if len > MAX_LIST {
        return Err(format!("`{key}` has {len} entries; at most {MAX_LIST}"));
    }
    Ok(())
}

/// Whether `id` is a valid tool id.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_NAME_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The namespaced name of a provided method, `tool.<id>.<name>` (ADR-1554): the namespace no
/// engine method uses, so a provided method never shadows one. It is called through
/// `editor.tool_call { tool, method }`, never as a top-level method.
pub fn qualified_name(tool: &str, name: &str) -> String {
    format!("tool.{tool}.{name}")
}

/// Whether `name` is a valid provided-method name.
#[must_use]
pub fn valid_method_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

fn valid_pattern(pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let body = pattern.strip_suffix('*').unwrap_or(pattern);
    !body.is_empty()
        && body.len() <= MAX_NAME_BYTES
        && body
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

/// Where a manifest was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestSource {
    /// `<project>/.regia/tools/`: `autostart` is ignored.
    Project,
    /// `<user dir>/tools/`.
    User,
    /// A directory of `REGIA_TOOL_PATH`.
    ToolPath,
}

impl ManifestSource {
    /// `project`, `user` or `tool_path`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::ToolPath => "tool_path",
        }
    }
}

/// One manifest file found by [`discover`]: the manifest, or why it was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredTool {
    /// The file.
    pub path: PathBuf,
    /// Which directory it came from.
    pub source: ManifestSource,
    /// The manifest, or the error.
    pub manifest: Result<ToolManifest, String>,
}

impl DiscoveredTool {
    /// The manifest's directory (relative `program` and `cwd` resolve against it).
    #[must_use]
    pub fn dir(&self) -> PathBuf {
        self.path.parent().map_or_else(PathBuf::new, Path::to_path_buf)
    }

    /// The tool id: the manifest's, or the file name's for a refused manifest.
    #[must_use]
    pub fn id(&self) -> String {
        match &self.manifest {
            Ok(manifest) => manifest.id.clone(),
            Err(_) => self
                .path
                .file_name()
                .and_then(|f| f.to_str())
                .map(|f| f.strip_suffix(MANIFEST_SUFFIX).unwrap_or(f).to_string())
                .unwrap_or_default(),
        }
    }

    /// Whether the manifest asks to start with the editor and may (outside the project).
    #[must_use]
    pub fn autostarts(&self) -> bool {
        self.source != ManifestSource::Project && self.manifest.as_ref().is_ok_and(|m| m.launch.autostart)
    }
}

/// The directories searched, in order: the project's, the user's, then `REGIA_TOOL_PATH`.
#[must_use]
pub fn discovery_dirs(project: Option<&Path>) -> Vec<(PathBuf, ManifestSource)> {
    let mut dirs = Vec::new();
    if let Some(project) = project {
        dirs.push((
            regia_core::brand::project_state_dir(project).join("tools"),
            ManifestSource::Project,
        ));
    }
    if let Some(user) = regia_platform::paths::user_dir() {
        dirs.push((user.join("tools"), ManifestSource::User));
    }
    if let Some(paths) = std::env::var_os(TOOL_PATH_ENV) {
        dirs.extend(
            std::env::split_paths(&paths)
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| (p, ManifestSource::ToolPath)),
        );
    }
    dirs
}

/// Reads every `*.tool.toml` in `dirs` (each directory sorted by file name). A later manifest
/// with an id already seen is refused as a duplicate. Missing directories are skipped.
#[must_use]
pub fn discover(dirs: &[(PathBuf, ManifestSource)]) -> Vec<DiscoveredTool> {
    let mut found: Vec<DiscoveredTool> = Vec::new();
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (dir, source) in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .file_name()
                        .and_then(|f| f.to_str())
                        .is_some_and(|f| f.ends_with(MANIFEST_SUFFIX))
            })
            .collect();
        paths.sort();
        for path in paths {
            let mut manifest = ToolManifest::load(&path);
            if let Ok(parsed) = &manifest
                && let Some(first) = seen.get(&parsed.id)
            {
                manifest = Err(format!(
                    "duplicate tool id `{}`: {} is used",
                    parsed.id,
                    first.display()
                ));
            }
            if let Ok(parsed) = &manifest {
                seen.insert(parsed.id.clone(), path.clone());
            }
            found.push(DiscoveredTool {
                path,
                source: *source,
                manifest,
            });
        }
    }
    found
}

/// The program to run: absolute as given, relative to `manifest_dir` when it has a path
/// separator, otherwise a bare name for `PATH` lookup.
#[must_use]
pub fn resolve_program(manifest_dir: &Path, program: &str) -> PathBuf {
    let path = Path::new(program);
    if path.is_absolute() || path.components().count() <= 1 {
        path.to_path_buf()
    } else {
        manifest_dir.join(path)
    }
}

/// The working directory: `cwd` relative to `manifest_dir` (absolute as given), default
/// `manifest_dir`.
#[must_use]
pub fn resolve_cwd(manifest_dir: &Path, cwd: Option<&str>) -> PathBuf {
    match cwd {
        Some(cwd) if Path::new(cwd).is_absolute() => PathBuf::from(cwd),
        Some(cwd) => manifest_dir.join(cwd),
        None => manifest_dir.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED_ROUGE: &str = r#"
format = 1
id = "red-rouge"
name = "Red-Rouge"
version = "0.4.0"
description = "Pick page, review queue and publish calendar."

[launch]
program = "uv"
args = ["run", "red-rouge", "editor-tool"]
autostart = true
env = { RED_ROUGE_PANEL = "1" }

[scope]
permissions = ["read", "play"]
methods = ["replay.*", "camera.*", "render.out_*", "scene.query_entities"]

[[provides]]
name = "pick.list"
description = "The pick page's scenes, newest first."

[[provides]]
name = "queue.approve"
permission = "edit_scene"
params = [{ name = "scene", kind = "string", required = true, description = "Scene id." }]

[panel]
title = "Red-Rouge"
url = "http://127.0.0.1:8765/?panel=1"
"#;

    #[test]
    fn a_full_manifest_parses() {
        let manifest = ToolManifest::parse(RED_ROUGE).expect("valid");
        assert_eq!(manifest.id, "red-rouge");
        assert_eq!(manifest.title(), "Red-Rouge");
        assert_eq!(manifest.launch.args, ["run", "red-rouge", "editor-tool"]);
        assert_eq!(manifest.scope_permissions(), [Permission::Read, Permission::Play]);
        let pick = manifest.provided("pick.list").expect("declared");
        assert_eq!(pick.permission(), Permission::Read, "read is the default");
        let approve = manifest.provided("queue.approve").expect("declared");
        assert_eq!(approve.permission(), Permission::EditScene);
        assert_eq!(approve.params[0].kind, "string");
        assert!(approve.params[0].required);
        let row = approve.to_json("red-rouge");
        assert_eq!(row["tool"], "red-rouge");
        assert_eq!(row["qualified"], "tool.red-rouge.queue.approve");
        assert_eq!(row["params"][0]["name"], "scene");
    }

    #[test]
    fn no_engine_method_is_in_the_tool_namespace() {
        // Provided methods are named `tool.<id>.<name>` and reached only through
        // `editor.tool_call`: no engine or editor method may use that prefix (design constraint 10).
        for method in regia_agent::schema::METHODS {
            assert!(
                !method.name.starts_with("tool."),
                "{} is in the tool namespace",
                method.name
            );
        }
        for (method, _) in crate::tools::agent::methods() {
            assert!(!method.name.starts_with("tool."), "{}", method.name);
        }
        assert!(qualified_name("t", "a.b").starts_with("tool.t."));
    }

    #[test]
    fn a_minimal_manifest_takes_defaults() {
        let manifest = ToolManifest::parse("format = 1\nid = \"tincal\"\n[launch]\nprogram = \"tincal\"\n")
            .expect("valid");
        assert_eq!(manifest.name, "tincal", "name defaults to the id");
        assert_eq!(manifest.title(), "tincal");
        assert!(manifest.scope.permissions.is_empty());
        assert!(manifest.provides.is_empty());
        assert!(!manifest.launch.autostart);
    }

    /// Each refusal names what is wrong; nothing is guessed.
    #[test]
    fn invalid_manifests_say_why() {
        let base = |extra: &str| format!("format = 1\nid = \"t\"\n{extra}\n[launch]\nprogram = \"p\"\n");
        let cases: Vec<(String, &str)> = vec![
            ("id = \"x\"\n[launch]\nprogram = \"p\"\n".into(), "`format = 1` is missing"),
            ("format = 2\nid = \"x\"\n[launch]\nprogram = \"p\"\nnew_key = 1\n".into(), "newer than this editor"),
            ("format = 0\nid = \"x\"\n[launch]\nprogram = \"p\"\n".into(), "not a manifest format"),
            ("format = 1\nid = \"Bad Id\"\n[launch]\nprogram = \"p\"\n".into(), "`id`"),
            ("format = 1\nid = \"t\"\n[launch]\nprogram = \" \"\n".into(), "`launch.program` is empty"),
            (base("typo = 1"), "invalid manifest"),
            (
                "format = 1\nid = \"t\"\n[launch]\nprogram = \"p\"\n[scope]\npermissions = [\"tools\"]\n".into(),
                "may not grant \"tools\"",
            ),
            (
                "format = 1\nid = \"t\"\n[launch]\nprogram = \"p\"\n[scope]\npermissions = [\"root\"]\n".into(),
                "unknown permission",
            ),
            (
                "format = 1\nid = \"t\"\n[launch]\nprogram = \"p\"\n[scope]\nmethods = [\"*scene\"]\n".into(),
                "pattern",
            ),
            (
                "format = 1\nid = \"t\"\n[launch]\nprogram = \"p\"\nenv = { REGIA_TOOL_TOKEN = \"x\" }\n".into(),
                "the launcher sets it",
            ),
            (
                "format = 1\nid = \"t\"\n[launch]\nprogram = \"p\"\nenv = { REGIA_PROJECT_DIR = \"x\" }\n".into(),
                "the launcher sets it",
            ),
            (
                "format = 1\nid = \"t\"\n[launch]\nprogram = \"p\"\n[[provides]]\nname = \"a\"\n[[provides]]\nname = \"a\"\n".into(),
                "twice",
            ),
            (
                "format = 1\nid = \"t\"\n[launch]\nprogram = \"p\"\n[[provides]]\nname = \".a\"\n".into(),
                "`provides` name",
            ),
            (
                "format = 1\nid = \"t\"\n[launch]\nprogram = \"p\"\n[[provides]]\nname = \"a\"\npermission = \"tools\"\n".into(),
                "must be read, edit_scene, play or assets",
            ),
            (
                "format = 1\nid = \"t\"\n[launch]\nprogram = \"p\"\n[[provides]]\nname = \"a\"\nparams = [{ name = \"x\", kind = \"int\" }]\n".into(),
                "kind",
            ),
            (
                "format = 1\nid = \"t\"\n[launch]\nprogram = \"p\"\n[panel]\nurl = \"file:///etc/passwd\"\n".into(),
                "http:// or https://",
            ),
            ("not toml at all [".into(), "TOML parse error"),
        ];
        for (text, needle) in cases {
            let error = ToolManifest::parse(&text).expect_err(&text);
            assert!(error.contains(needle), "{text:?}: {error}");
        }
    }

    #[test]
    fn names_and_patterns() {
        assert!(valid_id("red-rouge"));
        assert!(valid_id("tincal_2"));
        assert!(!valid_id(""));
        assert!(!valid_id("Red"));
        assert!(!valid_id(&"a".repeat(MAX_NAME_BYTES + 1)));
        assert!(valid_method_name("pick.list"));
        assert!(!valid_method_name("pick."));
        assert!(!valid_method_name("Pick"));
        assert!(valid_pattern("*"));
        assert!(valid_pattern("scene.*"));
        assert!(valid_pattern("render.out_*"));
        assert!(!valid_pattern("**"));
        assert!(!valid_pattern("a*b"));
    }

    #[test]
    fn programs_and_working_dirs_resolve_against_the_manifest() {
        let dir = Path::new("tools-dir");
        assert_eq!(resolve_program(dir, "uv"), PathBuf::from("uv"), "PATH lookup");
        assert_eq!(resolve_program(dir, "bin/tool"), dir.join("bin/tool"));
        let absolute = std::env::temp_dir().join("tool");
        assert_eq!(resolve_program(dir, &absolute.to_string_lossy()), absolute);
        assert_eq!(resolve_cwd(dir, None), dir);
        assert_eq!(resolve_cwd(dir, Some("work")), dir.join("work"));
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write manifest");
    }

    /// Discovery reads the project's, then later directories; the first id wins, bad files and
    /// misnamed files are listed with their errors, and only non-project manifests autostart.
    #[test]
    fn discovery_orders_reports_and_deduplicates() {
        let root = regia_test_utils::unique_temp_dir("tool-manifests");
        let project = root.join("project");
        let user = root.join("user");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::create_dir_all(&user).expect("user dir");
        write(&project.join("red-rouge.tool.toml"), RED_ROUGE);
        write(&project.join("broken.tool.toml"), "format = 1\n");
        write(&project.join("ignored.txt"), "not a manifest");
        write(&user.join("red-rouge.tool.toml"), RED_ROUGE);
        write(
            &user.join("misnamed.tool.toml"),
            "format = 1\nid = \"other\"\n[launch]\nprogram = \"p\"\n",
        );
        write(
            &user.join("tincal.tool.toml"),
            "format = 1\nid = \"tincal\"\n[launch]\nprogram = \"tincal\"\nautostart = true\n",
        );
        let dirs = vec![
            (project.clone(), ManifestSource::Project),
            (user.clone(), ManifestSource::User),
            (root.join("missing"), ManifestSource::ToolPath),
        ];
        let found = discover(&dirs);
        let summary: Vec<(String, ManifestSource, bool)> = found
            .iter()
            .map(|tool| (tool.id(), tool.source, tool.manifest.is_ok()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("broken".to_string(), ManifestSource::Project, false),
                ("red-rouge".to_string(), ManifestSource::Project, true),
                ("misnamed".to_string(), ManifestSource::User, false),
                ("red-rouge".to_string(), ManifestSource::User, false),
                ("tincal".to_string(), ManifestSource::User, true),
            ]
        );
        assert!(
            found[2]
                .manifest
                .as_ref()
                .expect_err("misnamed")
                .contains("must be named other.tool.toml")
        );
        assert!(
            found[3]
                .manifest
                .as_ref()
                .expect_err("duplicate")
                .contains("duplicate tool id")
        );
        assert!(!found[1].autostarts(), "a project manifest never autostarts");
        assert!(found[4].autostarts());
        assert_eq!(found[1].dir(), project);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_dirs_start_with_the_project() {
        let project = Path::new("some-project");
        let dirs = discovery_dirs(Some(project));
        assert_eq!(dirs[0].1, ManifestSource::Project);
        assert!(dirs[0].0.ends_with("tools"));
        assert!(
            discovery_dirs(None)
                .iter()
                .all(|(_, source)| *source != ManifestSource::Project)
        );
    }
}
