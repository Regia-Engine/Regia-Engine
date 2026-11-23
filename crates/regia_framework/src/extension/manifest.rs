// SPDX-License-Identifier: MPL-2.0
//! `regia-plugin.toml`, `format_version = 1` (`specs/57` § 4.1).

use std::collections::BTreeSet;

use semver::{Version, VersionReq};
use serde::Deserialize;

/// The manifest format version this build reads.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// Capabilities the host knows (spec § 2.4).
pub const KNOWN_CAPABILITIES: [&str; 11] = [
    "systems",
    "asset_loader",
    "asset_importer",
    "editor_panel",
    "agent_method",
    "render_pass",
    "settings",
    "content_mount",
    "feature_pack",
    "process",
    "network",
];

/// Capabilities granted by enabling (spec § 2.7).
pub const IMPLICIT_CAPABILITIES: [&str; 2] = ["systems", "settings"];

/// Load phases in startup order (spec § 2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Before the asset server starts.
    Foundation,
    /// Before engine default plugins finish `build`.
    Early,
    /// With game plugins.
    Default,
    /// After every `default` extension `build`.
    Late,
}

impl Phase {
    /// Lowercase name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Foundation => "foundation",
            Self::Early => "early",
            Self::Default => "default",
            Self::Late => "late",
        }
    }
}

/// Why a manifest was refused. `code()` is the lowercase protocol code (spec § 5.3).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    /// `format_version` is higher than this build reads; nothing was read (`manifest_too_new`).
    #[error("manifest_too_new: format_version {found} > {MANIFEST_FORMAT_VERSION}")]
    TooNew {
        /// The version found.
        found: i64,
    },
    /// A path leaves the extension directory (`path_escape`).
    #[error("path_escape: {field}: `{path}` leaves the extension directory")]
    PathEscape {
        /// The manifest field.
        field: String,
        /// The offending path.
        path: String,
    },
    /// The text is not a valid manifest (`manifest_invalid`).
    #[error("manifest_invalid: {0}")]
    Invalid(String),
}

impl ManifestError {
    /// The lowercase protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooNew { .. } => "manifest_too_new",
            Self::PathEscape { .. } => "path_escape",
            Self::Invalid(_) => "manifest_invalid",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEngine {
    min_version: Option<String>,
    max_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDependency {
    id: String,
    version: Option<String>,
    #[serde(default)]
    optional: bool,
    platforms: Option<Vec<String>>,
    profiles: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCode {
    #[serde(rename = "crate")]
    crate_name: Option<String>,
    audience: Option<Vec<String>>,
    phase: Option<Phase>,
    library: Option<String>,
    // The spec's example lists `capabilities` after the last `[[code]]`, where TOML attaches it to
    // that table. Accept it there and merge, so the example parses as written.
    capabilities: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContent {
    root: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTool {
    name: String,
    command: String,
    #[serde(default)]
    capabilities: Vec<String>,
    timeout_seconds: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct Raw {
    id: String,
    name: Option<String>,
    version: String,
    publisher: Option<String>,
    licence: Option<String>,
    description: Option<String>,
    #[serde(default)]
    auto_enable: bool,
    #[serde(default)]
    provides: Vec<String>,
    #[serde(default)]
    replaces: Vec<String>,
    #[serde(default)]
    conflicts: Vec<String>,
    platforms: Option<Vec<String>>,
    profiles: Option<Vec<String>>,
    #[serde(default)]
    capabilities: Vec<String>,
    engine: Option<RawEngine>,
    #[serde(default)]
    dependency: Vec<RawDependency>,
    #[serde(default)]
    code: Vec<RawCode>,
    #[serde(default)]
    content: Vec<RawContent>,
    #[serde(default)]
    tool: Vec<RawTool>,
}

const KNOWN_TOP_LEVEL: [&str; 20] = [
    "format_version",
    "id",
    "name",
    "version",
    "publisher",
    "licence",
    "description",
    "auto_enable",
    "provides",
    "replaces",
    "conflicts",
    "platforms",
    "profiles",
    "capabilities",
    "engine",
    "dependency",
    "code",
    "content",
    "tool",
    "package",
];

/// One dependency edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// Extension id.
    pub id: String,
    /// Accepted versions (`*` when the manifest names none).
    pub version: VersionReq,
    /// Whether the dependency may be absent.
    pub optional: bool,
    /// Platforms this edge applies to; `None` = all, empty = none.
    pub platforms: Option<Vec<String>>,
    /// Profiles this edge applies to; `None` = all, empty = none.
    pub profiles: Option<Vec<String>>,
}

/// One `[[code]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeEntry {
    /// Cargo package name (source extension).
    pub crate_name: Option<String>,
    /// `game`, `editor`, `server`, `tool`.
    pub audience: Vec<String>,
    /// Load phase.
    pub phase: Phase,
    /// Prebuilt library path (dynamic form).
    pub library: Option<String>,
}

/// One `[[tool]]` entry (out-of-process helper).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEntry {
    /// Tool name.
    pub name: String,
    /// Command relative to the extension directory.
    pub command: String,
    /// Requested capabilities.
    pub capabilities: Vec<String>,
    /// Kill-after seconds.
    pub timeout_seconds: u32,
}

/// A parsed, checked manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionManifest {
    /// Lowercase id `[a-z][a-z0-9_]{1,31}`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Semver version.
    pub version: Version,
    /// Publisher text.
    pub publisher: String,
    /// SPDX expression as written (not parsed).
    pub licence: String,
    /// Description text.
    pub description: String,
    /// Enabled for every project unless disabled.
    pub auto_enable: bool,
    /// Virtual capabilities provided.
    pub provides: Vec<String>,
    /// Ids whose `provides` this takes over.
    pub replaces: Vec<String>,
    /// Ids that cannot be enabled together with this one.
    pub conflicts: Vec<String>,
    /// Platform filter; `None` = all, empty = none.
    pub platforms: Option<Vec<String>>,
    /// Profile filter; `None` = all, empty = none.
    pub profiles: Option<Vec<String>>,
    /// Lowest engine version accepted.
    pub engine_min: Option<Version>,
    /// Highest engine version accepted (inclusive).
    pub engine_max: Option<Version>,
    /// Dependencies.
    pub dependencies: Vec<Dependency>,
    /// Code entries.
    pub code: Vec<CodeEntry>,
    /// Every requested capability, sorted and deduplicated (top level, `[[code]]` and tools
    /// merged; `process` when there is a `[[tool]]`), so a review sees the whole surface.
    pub capabilities: Vec<String>,
    /// The part of [`Self::capabilities`] loading the extension needs (top level and `[[code]]`).
    /// A capability only a `[[tool]]` lists gates that tool, not the extension (spec § 2.7).
    pub load_capabilities: Vec<String>,
    /// Content roots relative to the extension directory.
    pub content: Vec<String>,
    /// Out-of-process tools.
    pub tools: Vec<ToolEntry>,
    /// Non-fatal findings (`unknown_field`).
    pub warnings: Vec<String>,
}

/// Whether `id` is a valid extension id (`[a-z][a-z0-9_]{1,31}`).
#[must_use]
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    (2..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// A manifest path stays in the extension directory: `/`-separated, relative, and passing the
/// shared project-path rules (no absolute, drive, UNC or `:` prefix, no `..` or Windows `.. `/`...`
/// segment, no NUL, no device name; ADR-1816). `.` (the directory itself) is allowed.
fn path_ok(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && match regia_core::project_path::check_relative(path) {
            Ok(_) | Err(regia_core::project_path::ProjectPathError::Empty) => true,
            Err(_) => false,
        }
}

fn invalid(msg: impl Into<String>) -> ManifestError {
    ManifestError::Invalid(msg.into())
}

impl ExtensionManifest {
    /// Parses and checks manifest text.
    ///
    /// # Errors
    ///
    /// `manifest_too_new` for a higher `format_version` (checked before anything else is read),
    /// `path_escape` for a path leaving the directory, `manifest_invalid` for everything else:
    /// bad TOML, a bad id or version, unknown keys inside `[[code]]`, `[[dependency]]`, `[engine]`,
    /// `[[content]]` or `[[tool]]`.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let mut table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| invalid(e.message().to_owned()))?;
        match table.get("format_version") {
            Some(toml::Value::Integer(v)) if *v > i64::from(MANIFEST_FORMAT_VERSION) => {
                return Err(ManifestError::TooNew { found: *v });
            }
            Some(toml::Value::Integer(v)) if *v >= 1 => {}
            _ => return Err(invalid("format_version must be an integer >= 1")),
        }
        let mut warnings = Vec::new();
        let unknown: Vec<String> = table
            .keys()
            .filter(|k| !KNOWN_TOP_LEVEL.contains(&k.as_str()))
            .cloned()
            .collect();
        for k in unknown {
            table.remove(&k);
            warnings.push(format!("unknown_field: {k}"));
        }
        let raw: Raw = table
            .try_into()
            .map_err(|e: toml::de::Error| invalid(e.message().to_owned()))?;
        Self::from_raw(raw, warnings)
    }

    fn from_raw(raw: Raw, warnings: Vec<String>) -> Result<Self, ManifestError> {
        if !valid_id(&raw.id) {
            return Err(invalid(format!(
                "id `{}` must match [a-z][a-z0-9_]{{1,31}}",
                raw.id
            )));
        }
        let version = Version::parse(&raw.version).map_err(|e| invalid(format!("version: {e}")))?;
        let parse_v = |field: &str, v: &Option<String>| -> Result<Option<Version>, ManifestError> {
            v.as_deref()
                .map(|s| Version::parse(s).map_err(|e| invalid(format!("{field}: {e}"))))
                .transpose()
        };
        let (engine_min, engine_max) = match &raw.engine {
            Some(e) => (
                parse_v("engine.min_version", &e.min_version)?,
                parse_v("engine.max_version", &e.max_version)?,
            ),
            None => (None, None),
        };
        let mut dependencies = Vec::new();
        for d in &raw.dependency {
            if !valid_id(&d.id) {
                return Err(invalid(format!("dependency id `{}` is not a valid id", d.id)));
            }
            let version = match &d.version {
                Some(s) => VersionReq::parse(s)
                    .map_err(|e| invalid(format!("dependency `{}` version: {e}", d.id)))?,
                None => VersionReq::STAR,
            };
            dependencies.push(Dependency {
                id: d.id.clone(),
                version,
                optional: d.optional,
                platforms: d.platforms.clone(),
                profiles: d.profiles.clone(),
            });
        }
        for list in [&raw.provides, &raw.replaces, &raw.conflicts] {
            if let Some(bad) = list.iter().find(|s| s.is_empty()) {
                return Err(invalid(format!(
                    "empty entry `{bad}` in provides/replaces/conflicts"
                )));
            }
        }
        let mut code = Vec::new();
        let mut caps: BTreeSet<String> = raw.capabilities.iter().cloned().collect();
        for c in &raw.code {
            if let Some(lib) = &c.library
                && !path_ok(lib)
            {
                return Err(ManifestError::PathEscape {
                    field: "code.library".into(),
                    path: lib.clone(),
                });
            }
            if c.crate_name.is_none() && c.library.is_none() {
                return Err(invalid("a [[code]] entry needs `crate` or `library`"));
            }
            let audience = c.audience.clone().unwrap_or_else(|| vec!["game".to_owned()]);
            if let Some(bad) = audience
                .iter()
                .find(|a| !matches!(a.as_str(), "game" | "editor" | "server" | "tool"))
            {
                return Err(invalid(format!("unknown audience `{bad}`")));
            }
            caps.extend(c.capabilities.iter().flatten().cloned());
            code.push(CodeEntry {
                crate_name: c.crate_name.clone(),
                audience,
                phase: c.phase.unwrap_or(Phase::Default),
                library: c.library.clone(),
            });
        }
        let mut content = Vec::new();
        for c in &raw.content {
            if !path_ok(&c.root) {
                return Err(ManifestError::PathEscape {
                    field: "content.root".into(),
                    path: c.root.clone(),
                });
            }
            content.push(c.root.clone());
        }
        let load_capabilities: Vec<String> = caps.iter().cloned().collect();
        let mut tools = Vec::new();
        for t in &raw.tool {
            if !path_ok(&t.command) {
                return Err(ManifestError::PathEscape {
                    field: "tool.command".into(),
                    path: t.command.clone(),
                });
            }
            caps.insert("process".to_owned());
            caps.extend(t.capabilities.iter().cloned());
            tools.push(ToolEntry {
                name: t.name.clone(),
                command: t.command.clone(),
                capabilities: t.capabilities.clone(),
                timeout_seconds: t.timeout_seconds.unwrap_or(30),
            });
        }
        Ok(Self {
            name: raw.name.unwrap_or_else(|| raw.id.clone()),
            id: raw.id,
            version,
            publisher: raw.publisher.unwrap_or_default(),
            licence: raw.licence.unwrap_or_default(),
            description: raw.description.unwrap_or_default(),
            auto_enable: raw.auto_enable,
            provides: raw.provides,
            replaces: raw.replaces,
            conflicts: raw.conflicts,
            platforms: raw.platforms,
            profiles: raw.profiles,
            engine_min,
            engine_max,
            dependencies,
            code,
            capabilities: caps.into_iter().collect(),
            load_capabilities,
            content,
            tools,
            warnings,
        })
    }

    /// The earliest phase among the code entries (`default` for a content-only extension).
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.code.iter().map(|c| c.phase).min().unwrap_or(Phase::Default)
    }

    /// `content` for no code, `dynamic` when every code entry is a prebuilt library only,
    /// `source` otherwise.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        if self.code.is_empty() {
            "content"
        } else if self
            .code
            .iter()
            .all(|c| c.crate_name.is_none() && c.library.is_some())
        {
            "dynamic"
        } else {
            "source"
        }
    }
}
