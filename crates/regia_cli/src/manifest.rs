// SPDX-License-Identifier: MPL-2.0
//! `Regia.toml`, the project manifest (`docs/specs/21-tooling-and-packaging.md` § 2).
//!
//! Only the keys the CLI uses so far are modelled; every other key of the full manifest is ignored, so
//! projects written against the complete schema load unchanged.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// File name of the project manifest at the root of every Regia Engine project.
pub const MANIFEST_FILE: &str = regia_core::brand::MANIFEST_FILE;

/// The parsed project manifest.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Manifest {
    /// The `[project]` table.
    pub project: Project,
    /// The `[platforms.*]` tables.
    #[serde(default)]
    pub platforms: Platforms,
    /// The `[steam]` table (specs/24 § 3).
    #[serde(default)]
    pub steam: SteamConfig,
    /// The `[profiles.<name>]` tables: named build profiles (entry levels → dependency closure).
    #[serde(default)]
    pub profiles: BTreeMap<String, BuildProfile>,
    /// The `[extensions]` table (specs/57 § 2.6).
    #[serde(default)]
    pub extensions: regia_framework::extension::ExtensionsConfig,
    /// The `[procgen]` table (specs/64 § 7.3).
    #[serde(default)]
    pub procgen: ProcgenConfig,
    /// The `[lightmaps]` table (ADR-2024).
    #[serde(default)]
    pub lightmaps: LightmapsConfig,
    /// The `[kinds]` table (spec 101 § 5, ADR-2488).
    #[serde(default)]
    pub kinds: KindsConfig,
    /// The `[cook]` keys this manifest reads (spec 21 § 2): `texture_quality`. The cook reads its
    /// other keys (`chunks`, `shared_cache`, `textures`) itself.
    #[serde(default)]
    pub cook: CookConfig,
    /// The optional `[boot]` table, preserved in package metadata for runtime startup.
    #[serde(default)]
    pub boot: Option<toml::Table>,
    /// The optional `[game]` table (the pause key, ADR-10104), preserved in package metadata so a
    /// packaged game without a loose `Regia.toml` keeps it.
    #[serde(default)]
    pub game: Option<toml::Table>,
}

/// The `[cook]` table's keys the CLI manifest reads (spec 21 § 2, ADR-7217).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CookConfig {
    /// `final` (default) or `draft`: the quality of every texture whose `.meta` names none
    /// (`regia_asset_pipeline::ProjectTextureQuality`). `draft` cooks faster and smaller.
    pub texture_quality: String,
}

impl Default for CookConfig {
    fn default() -> Self {
        Self {
            texture_quality: regia_asset_pipeline::ProjectTextureQuality::Final
                .label()
                .to_string(),
        }
    }
}

impl CookConfig {
    /// The parsed `texture_quality`.
    ///
    /// # Errors
    /// A value other than `final` or `draft` (the message names the key).
    pub fn texture_quality(&self) -> Result<regia_asset_pipeline::ProjectTextureQuality> {
        regia_asset_pipeline::ProjectTextureQuality::from_label(&self.texture_quality).with_context(|| {
            format!(
                "[cook] texture_quality `{}` is not `final` or `draft`",
                self.texture_quality
            )
        })
    }
}

/// The `[kinds]` table: how strictly level entities are held to entity kinds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct KindsConfig {
    /// `off` (default), `warn` or `deny`: the kind lint in `regia preflight` and `regia cook`.
    pub strictness: regia_scene::kind::lint::Strictness,
}

/// The `[lightmaps]` table: what a shipping `regia package` holds a project's baked lightmaps to.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct LightmapsConfig {
    /// The largest chart-seam step (`LightmapSeams::p95`) a shipped entry may have. Lightmaps are
    /// opt-in: a project with no `.flmap` never meets this.
    pub seam_budget: f32,
    /// Per-tier lightmap rules (`[lightmaps.tiers.<name>]`, ADR-3120) over the default table
    /// (`regia_asset_pipeline::lightmap_tiers::LightmapTierTable::default`): the lightmap each tier
    /// package derives from the one bake.
    pub tiers: std::collections::BTreeMap<String, regia_asset_pipeline::lightmap_tiers::LightmapTierRule>,
}

impl Default for LightmapsConfig {
    fn default() -> Self {
        Self {
            seam_budget: regia_asset::lightmap::DEFAULT_SEAM_BUDGET,
            tiers: std::collections::BTreeMap::new(),
        }
    }
}

impl LightmapsConfig {
    /// The tier table: the defaults with this project's `[lightmaps.tiers]` over them.
    ///
    /// # Errors
    ///
    /// `lightmap_tier_rule:` for a bad tier name or a tier `padding` outside 1..=16 (ADR-5621).
    pub fn tier_table(&self) -> Result<regia_asset_pipeline::lightmap_tiers::LightmapTierTable> {
        regia_asset_pipeline::lightmap_tiers::LightmapTierTable::with_overrides(&self.tiers)
            .map_err(anyhow::Error::msg)
    }
}

/// The `[procgen]` table: project switches for procedural generation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ProcgenConfig {
    /// Whether graphs may change the heightfield at bake time (`terrain.flatten_road`). Off for a
    /// project whose terrain heights a game simulation owns.
    pub terrain_edits: bool,
}

/// One `[profiles.<name>]` table: a named build profile selected with `regia package --profile <name>`.
///
/// The profile ships exactly the dependency closure of its `entry_levels` (cooked assets through the
/// cooked files' dependency lists, raw files through the strings the levels contain) plus the
/// `include` globs, minus the `exclude` globs. It does **not** obfuscate or encrypt anything.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct BuildProfile {
    /// Cargo profile the binary is built with: `"shipping"` (default) or `"dev"`.
    pub base: String,
    /// Levels (paths under `assets/`, e.g. `levels/main.level.ron`) the package opens. Required.
    pub entry_levels: Vec<String>,
    /// Extra globs (`*`, `**`, `?`; paths under `assets/`) shipped in addition to the closure.
    pub include: Vec<String>,
    /// Globs removed from the package; the closure walk does not follow into them.
    pub exclude: Vec<String>,
    /// Cargo features passed to the game package (`--features`).
    pub features: Vec<String>,
    /// Pass `--no-default-features` to the game package.
    pub no_default_features: bool,
    /// Allow the agent server in a shipping binary (playtest builds). Default: a shipping package
    /// that contains the agent server fails `ship_check`.
    pub allow_agent: bool,
    /// `"server"` makes this a dedicated-server profile (spec 76 § 3); absent or `"client"` is a
    /// client package. Older tools ignore the key.
    pub target: Option<String>,
    /// The lightmap tier this package ships (`[lightmaps.tiers]`, ADR-3120): every lightmap in the
    /// pak is that tier's, derived from the bake; `None` ships the cooked lightmaps as they are.
    pub lightmap_tier: Option<String>,
}

impl Default for BuildProfile {
    fn default() -> Self {
        Self {
            base: "shipping".to_string(),
            entry_levels: Vec::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            features: Vec::new(),
            no_default_features: false,
            allow_agent: false,
            target: None,
            lightmap_tier: None,
        }
    }
}

impl BuildProfile {
    /// True for `target = "server"`.
    #[must_use]
    pub fn is_server(&self) -> bool {
        self.target.as_deref() == Some("server")
    }
}

/// The `[project]` table.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Project {
    /// Human-readable name, shown as the app label.
    pub name: String,
    /// Reverse-DNS identifier: Android `applicationId` and Java package of the generated activity.
    pub id: String,
    /// User-facing version (Android `versionName`).
    #[serde(default = "default_version")]
    pub version: String,
    /// Monotonic build number (Android `versionCode`).
    #[serde(default = "default_build_number")]
    pub build_number: u32,
}

/// The `[platforms]` table; Android's build fields and the packaging keys of the desktop tables
/// are modelled, every other key is ignored.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Platforms {
    /// `[platforms.android]`.
    #[serde(default)]
    pub android: AndroidPlatform,
    /// `[platforms.linux]`.
    #[serde(default)]
    pub linux: LinuxPlatform,
    /// `[platforms.windows]`.
    #[serde(default)]
    pub windows: DesktopPlatform,
}

/// `[platforms.linux]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LinuxPlatform {
    /// Formats `regia package --platform linux` builds when `--format` is not given: `tar`, `dir`,
    /// `appimage`. Empty means `["tar"]`.
    pub formats: Vec<String>,
    /// Render profile preset written to the package's `build_info.json` (`MobileHigh`, `DesktopHigh`, ...).
    pub render_profile: Option<String>,
}

/// `[platforms.windows]`: the keys `regia package` reads.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct DesktopPlatform {
    /// Render profile preset written to the package's `build_info.json`.
    pub render_profile: Option<String>,
}

impl Platforms {
    /// The `render_profile` configured for `platform` (`linux`, `windows`, `android`), canonical
    /// spelling, `None` when the table sets none.
    #[must_use]
    pub fn render_profile(&self, platform: &str) -> Option<&str> {
        let raw = match platform {
            "linux" => self.linux.render_profile.as_deref(),
            "windows" => self.windows.render_profile.as_deref(),
            "android" => self.android.render_profile.as_deref(),
            _ => None,
        }?;
        // A preset row or a tier with no row of its own (`VeryLow`, ADR-8844).
        regia_render::ProfileName::parse(raw).map(regia_render::ProfileName::name)
    }
}

/// `[platforms.android]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AndroidPlatform {
    /// `minSdk`; also the API level passed to `cargo ndk -P`.
    pub min_sdk: u32,
    /// `targetSdk` and `compileSdk`.
    pub target_sdk: u32,
    /// Screen orientation, in Regia.toml spelling (`sensor_landscape`, `portrait`, ...).
    pub orientation: Orientation,
    /// Android ABIs to build (`arm64-v8a`, `x86_64`, ...).
    pub abis: Vec<String>,
    /// Extra `<uses-permission>` entries, e.g. `android.permission.INTERNET`.
    pub permissions: Vec<String>,
    /// Cargo package of the project's Android `cdylib` crate (lib target `main`, `docs/02` § 10).
    pub cargo_package: String,
    /// Render profile preset written to the package's `build_info.json` (`MobileHigh`, ...).
    pub render_profile: Option<String>,
    /// Release signing keystore: `env:VAR` (the keystore file path comes from `VAR`) or a literal
    /// path. Release builds (APK/AAB `--profile shipping`) are signed with it; without it AGP 8
    /// leaves release builds unsigned (it never falls back to the debug keystore), so `regia
    /// package --profile shipping` errors. Dev builds use the AGP debug keystore and need no key.
    pub keystore: String,
    /// Keystore password, `env:VAR` or a literal.
    pub keystore_password: String,
    /// Signing key alias inside the keystore, `env:VAR` or a literal.
    pub key_alias: String,
    /// Key password, `env:VAR` or a literal. Defaults to the keystore password when empty.
    pub key_password: String,
    /// Launcher icon PNG, a project-relative path (specs/21 § 2). Unset: `assets/branding/icon.png`,
    /// then `assets/icon.png`.
    pub icon: Option<String>,
    /// Adaptive-icon foreground PNG, a project-relative path. Unset: `assets/branding/icon_fg.png`,
    /// then `assets/branding/icon_foreground.png`.
    pub adaptive_icon_foreground: Option<String>,
}

impl Default for AndroidPlatform {
    fn default() -> Self {
        Self {
            min_sdk: 29,
            target_sdk: 35,
            orientation: Orientation::SensorLandscape,
            abis: vec!["arm64-v8a".to_owned()],
            permissions: Vec::new(),
            cargo_package: "android".to_owned(),
            render_profile: None,
            keystore: String::new(),
            keystore_password: String::new(),
            key_alias: String::new(),
            key_password: String::new(),
            icon: None,
            adaptive_icon_foreground: None,
        }
    }
}

impl AndroidPlatform {
    /// Resolve a signing value written as `env:VAR` (the value comes from the environment variable
    /// `VAR`) or as a literal string. Empty/whitespace strings resolve to `None` (unset).
    ///
    /// Secrets (keystore passwords, key passwords) should be `env:` references so they are never
    /// committed to `Regia.toml`; the keystore path is typically `env:` too (specs/21 § 2).
    pub fn resolve_secret(spec: &str) -> Option<String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        if let Some(var) = spec.strip_prefix("env:") {
            return regia_core::brand::env_var(var)
                .ok()
                .filter(|v| !v.trim().is_empty());
        }
        Some(spec.to_owned())
    }

    /// Whether a release signing keystore is configured (its path resolves to a non-empty value).
    pub fn has_signing_keystore(&self) -> bool {
        Self::resolve_secret(&self.keystore).is_some()
    }
}

/// Screen orientation of the Android activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    /// Landscape, either way up, following the sensor.
    SensorLandscape,
    /// Fixed landscape.
    Landscape,
    /// Fixed portrait.
    Portrait,
    /// Portrait, either way up, following the sensor.
    SensorPortrait,
    /// Any orientation, following the sensor.
    #[serde(alias = "full_sensor")]
    Sensor,
}

impl Orientation {
    /// The `android:screenOrientation` value.
    pub fn android_value(self) -> &'static str {
        match self {
            Self::SensorLandscape => "sensorLandscape",
            Self::Landscape => "landscape",
            Self::Portrait => "portrait",
            Self::SensorPortrait => "sensorPortrait",
            Self::Sensor => "fullSensor",
        }
    }
}

/// The `[steam]` table of `Regia.toml` (specs/24 § 3): the Steam application id, the depot id
/// per platform, and the branches `regia steam upload` may publish to. All fields are optional
/// at parse time so non-Steam projects load unchanged; `regia package --steam` and
/// `regia steam upload` validate what they need.
///
/// ```toml
/// [steam]
/// app_id = 480
/// branches = ["beta", "playtest"]
///
/// [steam.depots]
/// linux = 480001
/// windows = 480002
/// ```
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SteamConfig {
    /// Steam application id (from the Steamworks partner site).
    #[serde(default)]
    pub app_id: u32,
    /// Depot id per Regia Engine platform name (`linux = 480001`). Valve's convention numbers a
    /// standard app's first depot `app_id + 1` (game-ci/steam-deploy).
    #[serde(default)]
    pub depots: std::collections::BTreeMap<String, u32>,
    /// Branches configured on the partner site that `regia steam upload --branch` accepts,
    /// besides `default`.
    #[serde(default)]
    pub branches: Vec<String>,
}

impl SteamConfig {
    /// Whether `[steam]` configures this app at all (has an app id).
    pub fn is_configured(&self) -> bool {
        self.app_id > 0
    }

    /// The depot id configured for `platform` (`"linux"`, `"windows"`).
    pub fn depot_of(&self, platform: &str) -> Option<u32> {
        self.depots.get(platform).copied()
    }
}

/// Formats `[platforms.linux] formats` may list.
pub const LINUX_FORMATS: &[&str] = &["tar", "dir", "appimage"];

/// ABIs that `cargo ndk` and the Android Gradle plugin both understand.
pub const KNOWN_ABIS: &[&str] = &["arm64-v8a", "armeabi-v7a", "x86_64", "x86"];
/// Java keywords and literals, which cannot be package name segments.
const JAVA_RESERVED: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "void",
    "volatile",
    "while",
    "_",
];

impl Manifest {
    /// Parses a manifest from TOML text and validates it.
    pub fn parse(text: &str) -> Result<Self> {
        let manifest: Manifest = toml::from_str(text).context("invalid Regia.toml")?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Loads `<project_dir>/Regia.toml` (or the legacy manifest name for one release, ADR-1388).
    pub fn load(project_dir: &Path) -> Result<Self> {
        let path = regia_core::brand::project_manifest(project_dir)
            .unwrap_or_else(|| project_dir.join(MANIFEST_FILE));
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "cannot read {} (is `{}` a Regia Engine project?)",
                path.display(),
                project_dir.display()
            )
        })?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    fn validate(&self) -> Result<()> {
        self.cook.texture_quality()?;
        // `p95 > NaN` is never true: a bad value would quietly switch the budget off.
        let budget = self.lightmaps.seam_budget;
        if !(budget.is_finite() && budget >= 0.0) {
            bail!("[lightmaps] seam_budget {budget} must be a number >= 0");
        }
        let tiers = self.lightmaps.tier_table().context("[lightmaps.tiers]")?;
        for (name, profile) in &self.profiles {
            if let Some(tier) = &profile.lightmap_tier {
                tiers
                    .rule(tier)
                    .map_err(anyhow::Error::msg)
                    .with_context(|| format!("[profiles.{name}] lightmap_tier"))?;
            }
        }
        if self.project.name.trim().is_empty() {
            bail!("[project] name must not be empty");
        }
        validate_app_id(&self.project.id)?;
        if self.project.build_number == 0 || self.project.build_number > 2_100_000_000 {
            bail!(
                "[project] build_number must be between 1 and 2100000000 (Android versionCode), got {}",
                self.project.build_number
            );
        }
        for (table, value) in [
            ("linux", &self.platforms.linux.render_profile),
            ("windows", &self.platforms.windows.render_profile),
            ("android", &self.platforms.android.render_profile),
        ] {
            if let Some(name) = value
                && regia_render::ProfileName::parse(name).is_none()
            {
                bail!(
                    "render_profile_unknown: [platforms.{table}] render_profile `{name}` is not a preset or tier; expected one of {}",
                    regia_render::ProfileName::names().join(", ")
                );
            }
        }
        for format in &self.platforms.linux.formats {
            if !LINUX_FORMATS.contains(&format.as_str()) {
                bail!(
                    "linux_format_unknown: [platforms.linux] formats entry `{format}`; expected one of {}",
                    LINUX_FORMATS.join(", ")
                );
            }
        }
        let android = &self.platforms.android;
        if android.min_sdk < 24 {
            bail!(
                "[platforms.android] min_sdk must be at least 24 (Vulkan), got {}",
                android.min_sdk
            );
        }
        if android.target_sdk < android.min_sdk {
            bail!(
                "[platforms.android] target_sdk ({}) must not be lower than min_sdk ({})",
                android.target_sdk,
                android.min_sdk
            );
        }
        if android.abis.is_empty() {
            bail!("[platforms.android] abis must list at least one ABI");
        }
        for abi in &android.abis {
            if !KNOWN_ABIS.contains(&abi.as_str()) {
                bail!(
                    "[platforms.android] unknown ABI `{abi}`; expected one of {}",
                    KNOWN_ABIS.join(", ")
                );
            }
        }
        for permission in &android.permissions {
            let valid = !permission.is_empty()
                && permission
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
            if !valid {
                bail!("[platforms.android] invalid permission name `{permission}`");
            }
        }
        if android.cargo_package.is_empty()
            || !android
                .cargo_package
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!(
                "[platforms.android] invalid cargo_package `{}`",
                android.cargo_package
            );
        }
        for (name, profile) in &self.profiles {
            validate_profile(name, profile)?;
        }
        let steam = &self.steam;
        if !steam.is_configured() && (!steam.depots.is_empty() || !steam.branches.is_empty()) {
            bail!(
                "[steam] has depots/branches but no app_id; the app id comes from the Steamworks \
                 partner site"
            );
        }
        for (platform, depot) in &steam.depots {
            if *depot == 0 {
                bail!("[steam.depots] {platform}: depot ids start at 1");
            }
            if !matches!(platform.as_str(), "linux" | "windows") {
                bail!(
                    "[steam.depots] unknown platform `{platform}`; Steam depots exist for \
                     `linux` and `windows` (macOS with the port)"
                );
            }
        }
        for branch in &steam.branches {
            if branch.trim().is_empty()
                || !branch
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                bail!("[steam] invalid branch name `{branch}`");
            }
        }
        Ok(())
    }
}

/// Names `[profiles.*]` may not redefine: the cargo profiles `regia package --profile` already knows.
pub const BUILTIN_PROFILES: &[&str] = &["dev", "shipping"];

/// The built-in dedicated-server profile (spec 76 § 3). A `[profiles.server]` table replaces it.
pub const BUILTIN_SERVER_PROFILE: &str = "server";

fn validate_profile(name: &str, profile: &BuildProfile) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("[profiles.{name}]: profile names use letters, digits, `_` and `-`");
    }
    if BUILTIN_PROFILES.contains(&name) {
        bail!("[profiles.{name}]: `{name}` is a built-in profile; pick another name");
    }
    if !BUILTIN_PROFILES.contains(&profile.base.as_str()) {
        bail!(
            "[profiles.{name}] base must be \"dev\" or \"shipping\", got `{}`",
            profile.base
        );
    }
    if !matches!(profile.target.as_deref(), None | Some("client" | "server")) {
        bail!(
            "[profiles.{name}] target must be \"client\" or \"server\", got `{}`",
            profile.target.as_deref().unwrap_or_default()
        );
    }
    if profile.entry_levels.is_empty() {
        bail!(
            "[profiles.{name}] needs at least one entry level (entry_levels = [\"levels/main.level.ron\"])"
        );
    }
    for feature in &profile.features {
        if matches!(feature.as_str(), "dev-assets" | "editor") || feature.ends_with("/dev-assets") {
            bail!(
                "[profiles.{name}] feature `{feature}` is dev/editor-only and may not be in a build profile"
            );
        }
        if !profile.allow_agent && (feature == "agent" || feature.ends_with("/agent")) {
            bail!(
                "[profiles.{name}] feature `{feature}` is the agent server; set allow_agent = true for a playtest build"
            );
        }
    }
    Ok(())
}

/// Checks that `id` is a valid Android application id that is also a valid Java package name
/// (the generated activity lives in that package).
pub fn validate_app_id(id: &str) -> Result<()> {
    let segments: Vec<&str> = id.split('.').collect();
    if segments.len() < 2 {
        bail!("[project] id `{id}` must have at least two segments, e.g. `com.example.mygame`");
    }
    for segment in segments {
        let mut chars = segment.chars();
        let starts_with_letter = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
        if !starts_with_letter || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!(
                "[project] id `{id}`: segment `{segment}` must start with a letter and contain only \
                 ASCII letters, digits and `_`"
            );
        }
        if JAVA_RESERVED.contains(&segment) {
            bail!("[project] id `{id}`: segment `{segment}` is a Java keyword");
        }
    }
    Ok(())
}

fn default_version() -> String {
    "0.1.0".to_owned()
}

fn default_build_number() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[project]
name = "Hello"
id = "com.example.hello"
"#;

    /// `[procgen] terrain_edits` (specs/64 § 7.3) is optional and off by default.
    #[test]
    fn procgen_terrain_edits_default_off_and_read() {
        assert!(!Manifest::parse(MINIMAL).expect("parse").procgen.terrain_edits);
        let on = format!("{MINIMAL}\n[procgen]\nterrain_edits = true\n");
        assert!(Manifest::parse(&on).expect("parse").procgen.terrain_edits);
    }

    /// The full example manifest of specs/21 § 2.
    #[test]
    fn load_reads_a_project_from_before_the_rename() {
        // Only the legacy manifest name (ADR-1388): still a project, still loads.
        let dir = regia_test_utils::unique_temp_dir("manifest-legacy");
        std::fs::write(dir.join(regia_core::brand::LEGACY_MANIFEST_FILE), MINIMAL).expect("write");
        assert!(regia_core::brand::is_project_root(&dir));
        assert_eq!(
            Manifest::load(&dir).expect("legacy manifest loads").project.name,
            "Hello"
        );
        // With both names present the current one wins.
        if MANIFEST_FILE != regia_core::brand::LEGACY_MANIFEST_FILE {
            std::fs::write(dir.join(MANIFEST_FILE), MINIMAL.replace("Hello", "Current")).expect("write");
            assert_eq!(Manifest::load(&dir).expect("load").project.name, "Current");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    const FULL_SPEC_EXAMPLE: &str = r#"
[project]
name = "My Game"
id = "com.example.mygame"
version = "1.0.0"
build_number = 1
engine = "0.1"
startup_level = "levels/main_menu.level.ron"
organization = "Example Studio"

[platforms.android]
min_sdk = 29
target_sdk = 35
orientation = "sensor_landscape"
abis = ["arm64-v8a"]
permissions = []
icon = "assets/branding/icon.png"
adaptive_icon_foreground = "assets/branding/icon_fg.png"
keystore = "env:REGIA_ANDROID_KEYSTORE"
render_profile = "MobileHigh"

[platforms.windows]
icon = "assets/branding/icon.png"
render_profile = "DesktopHigh"

[platforms.macos]
icon = "assets/branding/icon.png"
sign_identity = "env:REGIA_MACOS_IDENTITY"
notarize = false

[platforms.linux]
formats = ["tar"]

[cook]
chunks = { base = ["**"] }
texture_quality = "final"
"#;

    #[test]
    fn minimal_manifest_gets_defaults() -> Result<()> {
        let m = Manifest::parse(MINIMAL)?;
        assert_eq!(m.project.version, "0.1.0");
        assert_eq!(m.project.build_number, 1);
        assert_eq!(m.platforms.android, AndroidPlatform::default());
        let a = &m.platforms.android;
        assert_eq!((a.min_sdk, a.target_sdk), (29, 35));
        assert_eq!(a.orientation, Orientation::SensorLandscape);
        assert_eq!(a.abis, ["arm64-v8a"]);
        assert!(a.permissions.is_empty());
        assert_eq!(a.cargo_package, "android");
        Ok(())
    }

    /// A seam budget that could never fail is refused, not quietly off.
    #[test]
    fn a_bad_seam_budget_is_refused() {
        let with = |v: &str| Manifest::parse(&format!("{MINIMAL}\n[lightmaps]\nseam_budget = {v}\n"));
        assert_eq!(with("0.1").expect("parse").lightmaps.seam_budget, 0.1);
        assert_eq!(with("0.0").expect("parse").lightmaps.seam_budget, 0.0);
        for bad in ["nan", "inf", "-0.5"] {
            let e = with(bad).expect_err(bad);
            assert!(format!("{e:#}").contains("seam_budget"), "{e:#}");
        }
        assert_eq!(
            Manifest::parse(MINIMAL).expect("parse").lightmaps.seam_budget,
            regia_asset::lightmap::DEFAULT_SEAM_BUDGET
        );
    }

    #[test]
    fn full_spec_example_parses_and_unknown_keys_are_ignored() -> Result<()> {
        let m = Manifest::parse(FULL_SPEC_EXAMPLE)?;
        assert_eq!(m.project.name, "My Game");
        assert_eq!(m.project.id, "com.example.mygame");
        assert_eq!(m.project.version, "1.0.0");
        assert_eq!(m.platforms.android.target_sdk, 35);
        // `[cook] texture_quality` is read (ADR-7217), beside the keys the cook reads itself.
        assert_eq!(
            m.cook.texture_quality()?,
            regia_asset_pipeline::ProjectTextureQuality::Final
        );
        Ok(())
    }

    /// `[cook] texture_quality` (spec 21 § 2, ADR-7217): `final` by default, `draft` read, any
    /// other value refused with the key named.
    #[test]
    fn cook_texture_quality_is_read_and_checked() {
        use regia_asset_pipeline::ProjectTextureQuality as Q;
        let with = |table: &str| Manifest::parse(&format!("{MINIMAL}\n{table}"));
        assert_eq!(
            with("").expect("default").cook.texture_quality().expect("q"),
            Q::Final
        );
        assert_eq!(
            with("[cook]\ntexture_quality = \"draft\"\nchunks = { base = [\"**\"] }\n")
                .expect("draft")
                .cook
                .texture_quality()
                .expect("q"),
            Q::Draft
        );
        let err = with("[cook]\ntexture_quality = \"ultra\"\n").expect_err("ultra");
        assert!(format!("{err:#}").contains("texture_quality `ultra`"), "{err:#}");
    }

    #[test]
    fn android_values_are_read() -> Result<()> {
        let m = Manifest::parse(
            r#"
[project]
name = "X"
id = "org.x.y"
version = "2.3.4"
build_number = 42
[platforms.android]
min_sdk = 30
target_sdk = 36
orientation = "portrait"
abis = ["arm64-v8a", "x86_64"]
permissions = ["android.permission.INTERNET"]
cargo_package = "my-android"
"#,
        )?;
        assert_eq!(m.project.build_number, 42);
        let a = &m.platforms.android;
        assert_eq!((a.min_sdk, a.target_sdk), (30, 36));
        assert_eq!(a.orientation, Orientation::Portrait);
        assert_eq!(a.abis, ["arm64-v8a", "x86_64"]);
        assert_eq!(a.permissions, ["android.permission.INTERNET"]);
        assert_eq!(a.cargo_package, "my-android");
        Ok(())
    }

    #[test]
    fn orientation_mapping() -> Result<()> {
        let cases = [
            ("sensor_landscape", "sensorLandscape"),
            ("landscape", "landscape"),
            ("portrait", "portrait"),
            ("sensor_portrait", "sensorPortrait"),
            ("sensor", "fullSensor"),
            ("full_sensor", "fullSensor"),
        ];
        for (toml_value, android) in cases {
            let text = format!("{MINIMAL}\n[platforms.android]\norientation = \"{toml_value}\"\n");
            let m = Manifest::parse(&text)?;
            assert_eq!(
                m.platforms.android.orientation.android_value(),
                android,
                "{toml_value}"
            );
        }
        let bad = format!("{MINIMAL}\n[platforms.android]\norientation = \"sideways\"\n");
        assert!(Manifest::parse(&bad).is_err());
        Ok(())
    }

    #[test]
    fn missing_project_fields_are_errors() {
        assert!(Manifest::parse("[project]\nname = \"x\"\n").is_err());
        assert!(Manifest::parse("[project]\nid = \"a.b\"\n").is_err());
        assert!(Manifest::parse("").is_err());
    }

    #[test]
    fn app_id_validation() {
        for good in ["com.example.game", "a.b", "com.Example_2.x9"] {
            assert!(validate_app_id(good).is_ok(), "{good}");
        }
        for bad in [
            "game",
            "com..x",
            "com.1x",
            "com.my-game",
            "com.example.new",
            ".com.x",
            "com.x.",
            "çom.x",
        ] {
            assert!(validate_app_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn invalid_android_values_are_errors() {
        for table in [
            "abis = []",
            "abis = [\"mips\"]",
            "min_sdk = 21",
            "min_sdk = 30\ntarget_sdk = 29",
            "permissions = [\"android.permission.INTERNET\\\" evil=\\\"\"]",
            "cargo_package = \"\"",
            "cargo_package = \"a b\"",
        ] {
            let text = format!("{MINIMAL}\n[platforms.android]\n{table}\n");
            assert!(Manifest::parse(&text).is_err(), "{table}");
        }
        assert!(Manifest::parse(&format!("{MINIMAL}build_number = 0\n")).is_err());
    }

    #[test]
    fn steam_table_is_read_and_validated() -> Result<()> {
        // Absent: a non-Steam project loads with defaults.
        let m = Manifest::parse(MINIMAL)?;
        assert_eq!(m.steam, SteamConfig::default());
        assert!(!m.steam.is_configured());

        // Present and complete.
        let m = Manifest::parse(
            r#"
[project]
name = "X"
id = "org.x.y"

[steam]
app_id = 480
branches = ["beta", "playtest"]

[steam.depots]
linux = 480001
windows = 480002
"#,
        )?;
        assert!(m.steam.is_configured());
        assert_eq!(m.steam.depot_of("linux"), Some(480001));
        assert_eq!(m.steam.depot_of("windows"), Some(480002));
        assert_eq!(m.steam.depot_of("switch"), None);
        assert_eq!(m.steam.branches, ["beta", "playtest"]);
        Ok(())
    }

    #[test]
    fn invalid_steam_values_are_errors() {
        for table in [
            "[steam]\napp_id = 0\ndepots = { linux = 1 }",
            "[steam]\ndepots = { linux = 0 }",
            "[steam]\napp_id = 480\ndepots = { gamecube = 1 }",
            "[steam]\napp_id = 480\nbranches = [\"default live\"]",
            "[steam]\napp_id = 480\nbranches = [\"\"]",
        ] {
            let text = format!("{MINIMAL}\n{table}\n");
            assert!(Manifest::parse(&text).is_err(), "{table}");
        }
    }

    #[test]
    fn signing_secret_resolution() {
        // Literals pass through; empty/whitespace is unset.
        assert_eq!(AndroidPlatform::resolve_secret(""), None);
        assert_eq!(AndroidPlatform::resolve_secret("  "), None);
        assert_eq!(
            AndroidPlatform::resolve_secret("/keys/upload.jks"),
            Some("/keys/upload.jks".into())
        );
        assert_eq!(AndroidPlatform::resolve_secret("literal"), Some("literal".into()));
        // `env:` reads the variable; an unset variable resolves to None.
        assert_eq!(
            AndroidPlatform::resolve_secret("env:REGIA_TEST_UNSET_VAR_XYZ"),
            None
        );
        // A variable that is always present on the host resolves to its value.
        let path = regia_core::brand::env_var("PATH").expect("PATH is set");
        if !path.trim().is_empty() {
            assert_eq!(AndroidPlatform::resolve_secret("env:PATH"), Some(path));
        }
    }

    #[test]
    fn signing_keystore_detection() -> Result<()> {
        let m = Manifest::parse(MINIMAL)?;
        assert!(!m.platforms.android.has_signing_keystore());
        let m = Manifest::parse(&format!("{MINIMAL}[platforms.android]\nkeystore = \"/k.jks\"\n"))?;
        assert!(m.platforms.android.has_signing_keystore());
        let m = Manifest::parse(&format!(
            "{MINIMAL}[platforms.android]\nkeystore = \"env:REGIA_TEST_UNSET_VAR_XYZ\"\n"
        ))?;
        assert!(!m.platforms.android.has_signing_keystore());
        Ok(())
    }
}

#[cfg(test)]
mod template_application_id_tests {
    use super::*;

    #[test]
    fn every_java_reserved_suffix_is_rejected_by_template_naming() {
        for keyword in JAVA_RESERVED {
            assert!(
                regia_cli::new_project::crate_name_for(keyword).is_err(),
                "{keyword}"
            );
        }
    }
}
