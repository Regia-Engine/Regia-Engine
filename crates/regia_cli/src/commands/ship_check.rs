// SPDX-License-Identifier: MPL-2.0
//! Shipping-binary checks run by `regia package` (`docs/specs/21-tooling-and-packaging.md` § 4, "Shipping-binary checks"):
//!
//! - **glibc floor** (Linux): the newest `GLIBC_x.y` symbol version the binary imports must not be newer
//!   than the target runtime's glibc. Steam Linux Runtime 3.0 "sniper" (Debian 11, also what the Steam
//!   Deck runs native Linux games in) ships glibc 2.31; a binary linked on a current distro needs 2.4x and
//!   does not start there.
//! - **Shared libraries** (Linux): every `NEEDED` entry should be a library the Steam runtime provides
//!   (anything else is reported; X11, Wayland and Vulkan are `dlopen`ed and never appear here).
//! - **Build-machine strings** (all desktop platforms): a shipping binary must not contain the build
//!   machine's workspace, home or cargo-home paths, nor the dev asset watcher (`dev-assets`).
//! - **Cooked packs** (all platforms, ADR-1594): the staged paks load with no transcode (every
//!   texture in the platform's GPU family, no supercompressed KTX2, no legacy mesh;
//!   [`check_cooked_paks`]), the binary does not link the glTF / PNG importer
//!   ([`find_feature_markers`]) and the game's `Cargo.toml` does not depend on it
//!   ([`check_cargo_runtime_importers`]).
//! - **Lightmap seams** (all platforms, ADR-2024): no entry of a staged `.flmap` passes the project's
//!   chart-seam budget ([`check_lightmap_seams`]).
//!
//! - **Shader permutations** (all platforms, ADR-2026): every render profile the platform runs stays
//!   inside its material-graph pipeline budget ([`check_permutation_budget`], master plan § 5.5a).
//!
//! - **Windows subsystem** (Windows, ADR-2306): the shipped exe is a GUI-subsystem program, so the
//!   game opens no console window ([`check_windows_subsystem`]; the templates' mains ask for it in
//!   release builds). The PE header is read in Rust ([`pe_subsystem`]), no tool needed.
//! - **Windows resources** (Windows, ADR-2645): the shipped exe carries an icon (`RT_GROUP_ICON` and
//!   its `RT_ICON` images) and version info (`RT_VERSION`) whose `FileVersion` is the package
//!   version ([`check_windows_resources`]; the templates' `build.rs` embeds them). The resource
//!   directory is read in Rust ([`pe_resources`], [`parse_version_info`]).
//!
//! The glibc and `NEEDED` facts are read from the ELF bytes (`commands::elf_versions` and
//! `regia_framework`'s object reader, ADR-8835), so no binutils are needed; the `objdump` text
//! parsers stay as a test cross-check.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regia_asset::cooked_pack::{self, CookedPackFinding, PakSummary, Severity};
use serde::Serialize;

/// Checks the project's cooked shader-permutation list for `platform` against each profile's
/// budget (`permutations.json` in the cooked cache; enumerated from the asset database and saved
/// when a cache cooked before ADR-2026 has none, so the package still ships a list). Prints one
/// line per profile.
///
/// # Errors
///
/// `enforce` (a shipping package) and a profile over budget, or the list cannot be read.
pub fn check_permutation_budget(
    project_dir: &Path,
    platform: &str,
    enforce: bool,
) -> Result<regia_render::PermutationManifest> {
    use regia_asset_pipeline::permutation_cook;
    let cache_dir = regia_asset::cooked_cache_dir(project_dir, platform);
    let manifest = match permutation_cook::read_manifest(&cache_dir).map_err(anyhow::Error::msg)? {
        Some(manifest) => manifest,
        None => {
            let db = regia_asset_pipeline::AssetDatabase::open(project_dir)
                .with_context(|| format!("reading the asset database of {}", project_dir.display()))?;
            let mut warnings = Vec::new();
            let manifest =
                permutation_cook::project_permutations(project_dir, &db, &cache_dir, platform, &mut warnings);
            for warning in warnings {
                tracing::warn!("{warning}");
            }
            if let Err(err) = permutation_cook::write_manifest(&cache_dir, &manifest) {
                tracing::warn!("permutation manifest not written: {err}");
            }
            manifest
        }
    };
    check_permutation_manifest(&manifest, enforce)?;
    check_graph_layouts(project_dir, &cache_dir, platform, enforce)?;
    Ok(manifest)
}

/// Agent code of a material-graph pipeline set over a gate device class's binding limits, or of
/// cooked graph code a host shader rejects (ADR-4901; fails a shipping package).
pub const MATERIAL_GRAPH_LAYOUT_INVALID: &str = "material_graph_layout_invalid";

/// The ADR-4901 half of the permutation check: every distinct cooked graph code's bindings
/// against the limit class of each profile of `platform`
/// ([`regia_asset_pipeline::permutation_cook::graph_cook_checks`]). Prints its lines.
///
/// # Errors
///
/// `enforce` (a shipping package) and a problem ([`MATERIAL_GRAPH_LAYOUT_INVALID`]). A project
/// without an asset database is a warning (nothing was cooked to check).
pub fn check_graph_layouts(
    project_dir: &Path,
    cache_dir: &Path,
    platform: &str,
    enforce: bool,
) -> Result<()> {
    use regia_asset_pipeline::permutation_cook;
    let db = match regia_asset_pipeline::AssetDatabase::open(project_dir) {
        Ok(db) => db,
        Err(err) => {
            tracing::warn!("material graph layouts not checked: no asset database ({err})");
            return Ok(());
        }
    };
    if db.entries.is_empty() {
        // `AssetDatabase::open` returns an empty database when the file is missing or does not
        // decode, so the open error alone never carries this documented case: name it here.
        tracing::warn!("material graph layouts not checked: no asset database (nothing was cooked to check)");
        return Ok(());
    };
    let mut warnings = Vec::new();
    let checks = permutation_cook::graph_cook_checks(&db, cache_dir, platform, &mut warnings);
    for warning in &warnings {
        tracing::warn!("{warning}");
    }
    for line in &checks.lines {
        outln!("material graphs: {line}");
    }
    if checks.problems.is_empty() {
        return Ok(());
    }
    let text = format!(
        "{MATERIAL_GRAPH_LAYOUT_INVALID}: {platform}: {}",
        checks.problems.join("; ")
    );
    if enforce {
        bail!("{text}");
    }
    tracing::warn!("{text}");
    Ok(())
}

/// The budget check of [`check_permutation_budget`] on a manifest already read.
///
/// # Errors
///
/// `enforce` and a profile over budget.
pub fn check_permutation_manifest(manifest: &regia_render::PermutationManifest, enforce: bool) -> Result<()> {
    for profile in &manifest.profiles {
        outln!("permutations: {}", profile.summary_line());
    }
    let problems = manifest.budget_problems();
    if problems.is_empty() {
        return Ok(());
    }
    let text = format!(
        "shader permutations over budget ({}): {}. Merge material graphs, turn static switches into \
         parameters, or raise the budget in Regia.toml [cook.permutation_budget.<Profile>] \
         (master plan § 5.5a)",
        manifest.platform,
        problems.join("; ")
    );
    if enforce {
        bail!("{text}");
    }
    tracing::warn!("{text}");
    Ok(())
}

/// Agent code of a budget overrun on a platform the project ships (fails a shipping package).
pub const PERMUTATION_BUDGET_EXCEEDED: &str = "permutation_budget_exceeded";
/// Agent code of a budget overrun on a platform the project does not ship (a warning).
pub const PERMUTATION_BUDGET_OFF_TARGET: &str = "permutation_budget_off_target";

/// One permutation or shipped-shader budget overrun on one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetFinding {
    /// [`PERMUTATION_BUDGET_EXCEEDED`] (the platform is shipped) or [`PERMUTATION_BUDGET_OFF_TARGET`].
    pub code: &'static str,
    /// The platform tag.
    pub platform: String,
    /// The budget row that is over: `graph pipelines` or `shipped shader bytes`.
    pub kind: &'static str,
    /// Which profiles, and by how much.
    pub detail: String,
}

impl BudgetFinding {
    /// `code: platform kind over budget: detail`, the text of the error or warning.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "{}: {} {} over budget: {}",
            self.code, self.platform, self.kind, self.detail
        )
    }
}

/// Sorts every overrun in `manifests` (material-graph pipelines) and `costs` (shipped forward
/// WGSL bytes) by whether its platform is in `shipped` (ADR-2664, the ADR-2026 remainder): an
/// overrun on a shipped platform is [`PERMUTATION_BUDGET_EXCEEDED`], on any other platform
/// [`PERMUTATION_BUDGET_OFF_TARGET`].
#[must_use]
pub fn budget_findings(
    manifests: &[regia_render::PermutationManifest],
    costs: &[regia_render::ShaderCostReport],
    shipped: &[String],
) -> Vec<BudgetFinding> {
    let code_of = |platform: &str| {
        if shipped.iter().any(|p| p == platform) {
            PERMUTATION_BUDGET_EXCEEDED
        } else {
            PERMUTATION_BUDGET_OFF_TARGET
        }
    };
    let mut out = Vec::new();
    for manifest in manifests {
        let problems = manifest.budget_problems();
        if !problems.is_empty() {
            out.push(BudgetFinding {
                code: code_of(&manifest.platform),
                platform: manifest.platform.clone(),
                kind: "graph pipelines",
                detail: problems.join("; "),
            });
        }
    }
    for cost in costs {
        let problems = cost.budget_problems();
        if !problems.is_empty() {
            out.push(BudgetFinding {
                code: code_of(&cost.platform),
                platform: cost.platform.clone(),
                kind: "shipped shader bytes",
                detail: problems.join("; "),
            });
        }
    }
    out
}

/// The platforms `toml`'s `[cook] ship_platforms` lists plus `packaging`. A `ship_platforms` key
/// that does not parse is not "ship nothing": it is named in a warning and ignored, so a typoed
/// key does not silently downgrade every overrun to a warning.
fn shipped_platforms(toml: &str, packaging: &str) -> Vec<String> {
    let mut shipped = match regia_asset_pipeline::permutation_cook::ship_platforms(toml) {
        Some(platforms) => platforms,
        None => {
            let key_present = toml.parse::<toml::Table>().ok().is_some_and(|table| {
                table
                    .get("cook")
                    .is_some_and(|cook| cook.get("ship_platforms").is_some())
            });
            if key_present {
                tracing::warn!(
                    "[cook] ship_platforms is not a list of strings; ignored (only `{packaging}` ships)"
                );
            }
            Vec::new()
        }
    };
    if !shipped.iter().any(|p| p == packaging) {
        shipped.push(packaging.to_string());
    }
    shipped
}

/// The off-target half of the permutation budget check (ADR-2664) plus the shipped-shader-bytes
/// row: enumerates the project's graphs for every platform the cook knows
/// ([`regia_render::permutation_policy::SHIPPED_PLATFORMS`]) and measures the forward shaders of each,
/// then reports each overrun per [`budget_findings`]. The project ships `packaging` and what
/// `Regia.toml` `[cook] ship_platforms` lists. Overruns on a shipped platform fail when `enforce`
/// (a shipping package) and warn otherwise; overruns elsewhere always warn.
///
/// The graph-pipeline row of `packaging` itself is checked by [`check_permutation_budget`]; this
/// one covers its shader bytes and the other platforms.
///
/// # Errors
///
/// `enforce` and an overrun on a shipped platform.
pub fn check_permutation_platforms(project_dir: &Path, packaging: &str, enforce: bool) -> Result<()> {
    use regia_asset_pipeline::permutation_cook;
    let toml = std::fs::read_to_string(project_dir.join("Regia.toml")).unwrap_or_default();
    let shipped = shipped_platforms(&toml, packaging);
    let db = regia_asset_pipeline::AssetDatabase::open(project_dir)
        .with_context(|| format!("reading the asset database of {}", project_dir.display()))?;
    // Graph codes do not depend on the platform: the packaged platform's cooked cache serves all.
    let cache_dir = regia_asset::cooked_cache_dir(project_dir, packaging);
    let mut warnings = Vec::new();
    let mut manifests = Vec::new();
    for platform in regia_render::permutation_policy::SHIPPED_PLATFORMS {
        if platform != packaging {
            manifests.push(permutation_cook::project_permutations(
                project_dir,
                &db,
                &cache_dir,
                platform,
                &mut warnings,
            ));
        }
    }
    let costs: Vec<_> = regia_render::permutation_policy::SHIPPED_PLATFORMS
        .into_iter()
        .map(|platform| permutation_cook::project_shader_cost(project_dir, platform, &mut warnings))
        .collect();
    for warning in warnings {
        tracing::warn!("{warning}");
    }
    let findings = budget_findings(&manifests, &costs, &shipped);
    let mut failures = Vec::new();
    for finding in &findings {
        let text = finding.message();
        if finding.code == PERMUTATION_BUDGET_EXCEEDED && enforce {
            failures.push(text);
        } else {
            tracing::warn!("{text}");
            eprintln!("warn {text}");
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "{}. Raise the budget in Regia.toml [cook.permutation_budget.<Profile>] or shrink the shaders \
             (master plan § 5.5a)",
            failures.join("; ")
        )
    }
}

/// glibc of Steam Linux Runtime 3.0 "sniper" (Debian 11): the default floor of Linux shipping packages.
pub const STEAM_RUNTIME_GLIBC: GlibcVersion = GlibcVersion { major: 2, minor: 31 };

/// Shared libraries Steam Linux Runtime 3.0 provides (sonames). A `NEEDED` entry outside this list
/// is reported: the game may still run on a desktop distro but not inside the runtime.
pub const STEAM_RUNTIME_LIBS: &[&str] = &[
    "ld-linux-x86-64.so.2",
    "libc.so.6",
    "libm.so.6",
    "libdl.so.2",
    "libpthread.so.0",
    "librt.so.1",
    "libgcc_s.so.1",
    "libstdc++.so.6",
    "libz.so.1",
    "libasound.so.2",
    "libpulse.so.0",
    "libudev.so.1",
    "libdbus-1.so.3",
    "libX11.so.6",
    "libX11-xcb.so.1",
    "libxcb.so.1",
    "libXcursor.so.1",
    "libXrandr.so.2",
    "libXi.so.6",
    "libxkbcommon.so.0",
    "libxkbcommon-x11.so.0",
    "libwayland-client.so.0",
    "libwayland-cursor.so.0",
    "libwayland-egl.so.1",
    "libvulkan.so.1",
    "libsteam_api.so",
];

/// A glibc symbol version (`GLIBC_2.31` → 2.31).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GlibcVersion {
    /// Major version (2).
    pub major: u32,
    /// Minor version.
    pub minor: u32,
}

impl GlibcVersion {
    /// Parses `2.31` (a trailing patch, `2.3.4`, is ignored: glibc floors are `major.minor`).
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        Some(Self { major, minor })
    }
}

impl std::fmt::Display for GlibcVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// What [`check_linux_binary`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxBinaryReport {
    /// Newest `GLIBC_x.y` version the binary imports (`None`: no glibc imports, a static binary).
    pub max_glibc: Option<GlibcVersion>,
    /// The library that version is needed from (`libc.so.6`; `None` with `max_glibc`).
    pub max_glibc_from: Option<String>,
    /// `NEEDED` sonames, in file order.
    pub needed: Vec<String>,
    /// `NEEDED` sonames that Steam Linux Runtime 3.0 does not provide.
    pub outside_runtime: Vec<String>,
}

/// Newest `GLIBC_x.y` symbol version in `objdump -T` output (the text form of
/// [`linux_binary_report`]'s ELF reading, kept to cross-check it against binutils).
#[cfg(test)]
pub fn max_glibc_version(objdump_t: &str) -> Option<GlibcVersion> {
    objdump_t
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter_map(|token| token.strip_prefix("GLIBC_"))
        .filter_map(GlibcVersion::parse)
        .max()
}

/// `NEEDED` sonames in `objdump -p` output (test cross-check, as [`max_glibc_version`]).
#[cfg(test)]
pub fn needed_libraries(objdump_p: &str) -> Vec<String> {
    objdump_p
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            match (words.next(), words.next()) {
                (Some("NEEDED"), Some(lib)) => Some(lib.to_owned()),
                _ => None,
            }
        })
        .collect()
}

/// Checks an ELF binary against a glibc floor and the Steam runtime's library set.
///
/// The version-needed table and `DT_NEEDED` are read from the file's bytes
/// ([`elf_versions`](crate::commands::elf_versions), `regia_framework`'s object reader), so the
/// check needs no binutils (ADR-8835; it ran `objdump -T` and `objdump -p` before).
///
/// # Errors
///
/// The binary cannot be read or is no ELF64 little-endian file, or it imports glibc symbols newer
/// than `floor`.
pub fn check_linux_binary(binary: &Path, floor: GlibcVersion) -> Result<LinuxBinaryReport> {
    let bytes = std::fs::read(binary).with_context(|| format!("reading {}", binary.display()))?;
    let report = linux_binary_report(&bytes)
        .with_context(|| format!("reading the ELF tables of {}", binary.display()))?;
    if let Some(max) = report.max_glibc
        && max > floor
    {
        bail!(
            "{} needs GLIBC_{max}{} but the package targets glibc {floor} (Steam Linux Runtime 3.0 \
             \"sniper\" has {STEAM_RUNTIME_GLIBC}). Build with the glibc floor (`--glibc {floor}`, \
             cargo-zigbuild, or `--container steam-sniper|debian12`) instead of the host's glibc",
            binary.display(),
            report
                .max_glibc_from
                .as_deref()
                .map_or_else(String::new, |lib| format!(" (from {lib})")),
        );
    }
    Ok(report)
}

/// The glibc and library facts of an ELF binary's bytes (no floor applied).
///
/// # Errors
///
/// The bytes are no ELF64 little-endian file with section headers, or a table points outside it.
pub fn linux_binary_report(bytes: &[u8]) -> Result<LinuxBinaryReport> {
    let needs = crate::commands::elf_versions::version_needs(bytes)?;
    let newest = crate::commands::elf_versions::newest_glibc_need(&needs);
    let max_glibc = newest.map(|(version, _)| version);
    let max_glibc_from = newest.map(|(_, need)| need.file.clone());
    let needed = regia_framework::extension::objfile::ObjFile::parse(bytes)
        .and_then(|file| file.needed_libraries())
        .map_err(|err| anyhow::anyhow!("reading DT_NEEDED: {err}"))?;
    let outside_runtime = needed
        .iter()
        .filter(|lib| !STEAM_RUNTIME_LIBS.contains(&lib.as_str()))
        .cloned()
        .collect();
    Ok(LinuxBinaryReport {
        max_glibc,
        max_glibc_from,
        needed,
        outside_runtime,
    })
}

/// Byte strings a shipping binary must not contain, with the reason each is forbidden.
///
/// `workspace`, `home` and `cargo_home` are the build machine's directories (a packaged game that
/// embeds them leaks the build machine's layout and may read sources at runtime). The `dev-assets`
/// marker is the asset watcher's opt-out variable, which only a `dev-assets` build contains.
pub fn forbidden_strings(
    workspace: &Path,
    home: Option<&Path>,
    cargo_home: Option<&Path>,
) -> Vec<(String, &'static str)> {
    let mut out = vec![(
        workspace.to_string_lossy().into_owned(),
        "the build machine's workspace path",
    )];
    for (dir, why) in [
        (cargo_home, "the build machine's cargo home"),
        (home, "the build machine's home directory"),
    ] {
        // `/` or `/root`-sized prefixes would match unrelated strings; real homes are longer.
        if let Some(dir) = dir.map(|d| d.to_string_lossy().into_owned())
            && dir.len() > 6
        {
            out.push((dir, why));
        }
    }
    out.push((
        "REGIA_ASSET_WATCH".to_owned(),
        "the dev asset watcher (`dev-assets` feature) is linked",
    ));
    out
}

/// The `needles` that occur in `haystack`, as `"<needle>" (<reason>)` lines.
pub fn find_forbidden(haystack: &[u8], needles: &[(String, &'static str)]) -> Vec<String> {
    needles
        .iter()
        .filter(|(needle, _)| contains(haystack, needle.as_bytes()))
        .map(|(needle, why)| format!("\"{needle}\" ({why})"))
        .collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    let mut from = 0;
    while let Some(offset) = haystack[from..].iter().position(|&b| b == first) {
        let at = from + offset;
        if haystack[at + 1..].starts_with(rest) {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Fails when the shipping binary at `binary` contains any of the build machine's paths or the dev
/// asset watcher.
///
/// # Errors
///
/// The binary cannot be read, or it contains a forbidden string.
pub fn check_no_build_machine_strings(binary: &Path, workspace: &Path) -> Result<()> {
    let bytes = std::fs::read(binary).with_context(|| format!("reading {}", binary.display()))?;
    // `HOME`, else `USERPROFILE` (Windows hosts set only that one).
    let home = crate::android::toolchain::home_dir();
    let cargo_home = regia_core::brand::env_var_os("CARGO_HOME").map(std::path::PathBuf::from);
    let found = find_forbidden(
        &bytes,
        &forbidden_strings(workspace, home.as_deref(), cargo_home.as_deref()),
    );
    if !found.is_empty() {
        bail!(
            "the shipping binary {} contains build-machine strings:\n  {}\n\
             Shipping builds remap source paths (`--remap-path-prefix`) and must not enable \
             `regia/dev-assets` (check the game's Cargo.toml features)",
            binary.display(),
            found.join("\n  ")
        );
    }
    Ok(())
}

/// Thread name only the agent server (`regia_agent`, the `agent` feature) contains; its presence in
/// a binary means the protocol server is linked.
pub const AGENT_SERVER_MARKER: &str = "regia-agent-accept";

/// Byte strings that mean the editor is linked into the binary (its type paths and panic locations).
const EDITOR_MARKERS: &[&str] = &["regia_editor::", "regia_editor/src"];

/// Byte strings that mean the source importer is linked into the binary: the cook crate (its type
/// paths and panic locations) or the glTF parser (`gltf-json`'s panic locations, which keep the
/// crate's directory name after path remapping). A game that links either can parse glTF or decode
/// PNG at runtime instead of loading its cooked paks (ADR-1594).
pub const IMPORTER_MARKERS: &[&str] = &["regia_asset_pipeline::", "regia_asset_pipeline/src", "gltf-json-"];

/// Cooked type paths that runtime crates carry as plain string constants. A cooked file's type tag hashes its asset
/// type's original path, and some of those paths name the cook crate, so these match [`IMPORTER_MARKERS`] without
/// any importer code being linked. [`find_feature_markers`] masks exactly these strings before it scans (ADR-5382).
pub const RUNTIME_TYPE_PATHS: &[&str] = &[regia_terrain::stream::TILED_HEIGHTFIELD_TYPE_PATH];

/// `haystack` with every occurrence of each of `exact` zeroed (borrowed when none occurs).
fn mask_exact<'a>(haystack: &'a [u8], exact: &[&str]) -> std::borrow::Cow<'a, [u8]> {
    let mut masked: Option<Vec<u8>> = None;
    for needle in exact.iter().map(|s| s.as_bytes()).filter(|n| !n.is_empty()) {
        let mut from = 0;
        while let Some(at) = position(&haystack[from..], needle).map(|i| i + from) {
            masked.get_or_insert_with(|| haystack.to_vec())[at..at + needle.len()].fill(0);
            from = at + needle.len();
        }
    }
    masked.map_or(std::borrow::Cow::Borrowed(haystack), std::borrow::Cow::Owned)
}

/// The offset of the first `needle` in `haystack`.
fn position(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, rest) = needle.split_first()?;
    let mut from = 0;
    while let Some(offset) = haystack[from..].iter().position(|&b| b == first) {
        let at = from + offset;
        if haystack[at + 1..].starts_with(rest) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// The feature markers found in `haystack`: the agent server (unless `allow_agent`), the editor, the
/// dev asset watcher and the source importer ([`IMPORTER_MARKERS`], after masking [`RUNTIME_TYPE_PATHS`]), each
/// as a `"<marker>" (<reason>)` line.
pub fn find_feature_markers(haystack: &[u8], allow_agent: bool) -> Vec<String> {
    let haystack = mask_exact(haystack, RUNTIME_TYPE_PATHS);
    let haystack: &[u8] = &haystack;
    let mut needles: Vec<(String, &'static str)> = vec![(
        "REGIA_ASSET_WATCH".to_owned(),
        "the dev asset watcher (`dev-assets` feature) is linked",
    )];
    if !allow_agent {
        needles.push((
            AGENT_SERVER_MARKER.to_owned(),
            "the agent server (`agent` feature) is linked",
        ));
    }
    for marker in EDITOR_MARKERS {
        needles.push(((*marker).to_owned(), "editor code is linked"));
    }
    for marker in IMPORTER_MARKERS {
        needles.push((
            (*marker).to_owned(),
            "the glTF/PNG importer is linked (a runtime source parse; ship cooked paks instead)",
        ));
    }
    find_forbidden(haystack, &needles)
}

/// Fails when the shipping binary at `binary` links the agent server (unless `allow_agent`), the
/// editor or the dev asset watcher. `enforce` is true for named build profiles; the plain `shipping`
/// profile only warns, because games on `regia`'s default features (which include `agent`) shipped
/// that way before profiles existed.
///
/// # Errors
///
/// The binary cannot be read, or (when `enforce`) it contains a feature marker.
pub fn check_binary_features(binary: &Path, allow_agent: bool, enforce: bool) -> Result<()> {
    let bytes = std::fs::read(binary).with_context(|| format!("reading {}", binary.display()))?;
    let found = find_feature_markers(&bytes, allow_agent);
    if !found.is_empty() && !enforce {
        tracing::warn!(
            "the shipping binary {} contains features a shipped game should not have: {}. \
             Use `default-features = false` on `regia`, or a named `[profiles.<name>]`, which enforces this",
            binary.display(),
            found.join("; ")
        );
    } else if !found.is_empty() {
        bail!(
            "the shipping binary {} contains features a shipped game must not have:\n  {}\n\
             Build the game without them (`regia = {{ default-features = false, features = [\"ui\"] }}` \
             in its Cargo.toml; a `[profiles.<name>]` entry in Regia.toml can set `features` / \
             `no_default_features`). A playtest build may keep the agent with `allow_agent = true`.",
            binary.display(),
            found.join("\n  ")
        );
    }
    Ok(())
}

/// Fails when the game's `Cargo.toml` text turns on `regia`'s `agent`, `dev-assets` or `editor`
/// feature explicitly (a static check that works before building, and on Android where the binary is
/// not scanned). The default feature set is covered by [`check_binary_features`].
///
/// # Errors
///
/// `cargo_toml` does not parse, or a dependency on `regia` enables a forbidden feature.
pub fn check_cargo_features(cargo_toml: &str, allow_agent: bool) -> Result<()> {
    let doc: toml::Value = toml::from_str(cargo_toml).context("invalid Cargo.toml")?;
    let Some((_, dep)) = doc.get("dependencies").and_then(engine_dependency) else {
        return Ok(());
    };
    let features = dep
        .get("features")
        .and_then(toml::Value::as_array)
        .map(|a| a.iter().filter_map(toml::Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let bad: Vec<&str> = features
        .into_iter()
        .filter(|f| {
            matches!(*f, "dev-assets" | "editor")
                || (matches!(*f, "agent" | "functional-tests") && !allow_agent)
        })
        .collect();
    if !bad.is_empty() {
        bail!(
            "the game's Cargo.toml enables regia feature(s) {} which a shipping package must not contain",
            bad.join(", ")
        );
    }
    Ok(())
}

/// Crates whose presence in a game's `[dependencies]` means it can parse glTF or decode PNG at
/// runtime: the engine's cook crate and the glTF parser. Build-time use (`[build-dependencies]`, a
/// cook step in `build.rs`) and tests (`[dev-dependencies]`) are fine.
pub const RUNTIME_IMPORTER_CRATES: &[&str] = &["regia_asset_pipeline", "gltf", "gltf-json"];

/// The [`RUNTIME_IMPORTER_CRATES`] the game's `Cargo.toml` text lists under `[dependencies]` or a
/// `[target.'cfg(..)'.dependencies]` table (by key or by `package = "..."` rename), sorted.
///
/// # Errors
///
/// `cargo_toml` does not parse.
pub fn runtime_importer_dependencies(cargo_toml: &str) -> Result<Vec<String>> {
    let doc: toml::Value = toml::from_str(cargo_toml).context("invalid Cargo.toml")?;
    let mut tables: Vec<&toml::Value> = doc.get("dependencies").into_iter().collect();
    if let Some(targets) = doc.get("target").and_then(toml::Value::as_table) {
        tables.extend(targets.values().filter_map(|t| t.get("dependencies")));
    }
    let mut found: Vec<String> = tables
        .into_iter()
        .filter_map(toml::Value::as_table)
        .flat_map(|table| table.iter())
        .filter_map(|(key, dep)| {
            let name = dep.get("package").and_then(toml::Value::as_str).unwrap_or(key);
            RUNTIME_IMPORTER_CRATES.contains(&name).then(|| name.to_owned())
        })
        .collect();
    found.sort();
    found.dedup();
    Ok(found)
}

/// Fails when the game's `Cargo.toml` makes the glTF / PNG importer a runtime dependency
/// ([`runtime_importer_dependencies`]); with `enforce` false (the plain `shipping` profile) it only
/// warns, as [`check_binary_features`] does.
///
/// # Errors
///
/// `cargo_toml` does not parse, or (when `enforce`) it lists an importer crate.
pub fn check_cargo_runtime_importers(cargo_toml: &str, enforce: bool) -> Result<()> {
    let found = runtime_importer_dependencies(cargo_toml)?;
    if found.is_empty() {
        return Ok(());
    }
    let why = format!(
        "the game's Cargo.toml makes {} a runtime dependency: the shipped game could parse glTF or decode \
         PNG at load instead of reading its cooked paks. Move it to [build-dependencies] or \
         [dev-dependencies] and load assets through `AssetServer` (book: Cooked packs)",
        found.join(", ")
    );
    if enforce {
        bail!(why);
    }
    tracing::warn!("{why}");
    Ok(())
}

/// The PE `Subsystem` value of a Windows GUI program (`IMAGE_SUBSYSTEM_WINDOWS_GUI`): no console.
pub const PE_SUBSYSTEM_WINDOWS_GUI: u16 = 2;

/// The PE `Subsystem` value of a Windows console program (`IMAGE_SUBSYSTEM_WINDOWS_CUI`): Windows
/// opens a console window when it starts outside a terminal.
pub const PE_SUBSYSTEM_WINDOWS_CUI: u16 = 3;

/// The `Subsystem` field of a PE image (Microsoft's published "PE Format" specification): the DOS
/// header's `e_lfanew` (offset 0x3c) points at the `PE\0\0` signature, the 20-byte COFF file header
/// follows (`SizeOfOptionalHeader` at its offset 16), then the optional header, whose magic is 0x10b
/// (PE32) or 0x20b (PE32+); both keep `Subsystem` at offset 68 of it.
///
/// # Errors
///
/// `bytes` is not a PE image, or its headers are truncated.
pub fn pe_subsystem(bytes: &[u8]) -> Result<u16> {
    let u16_at = |at: usize| -> Option<u16> {
        bytes
            .get(at..at.checked_add(2)?)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    if bytes.get(..2) != Some(b"MZ".as_slice()) {
        bail!("not a PE image (no MZ header)");
    }
    let lfanew = bytes
        .get(0x3c..0x40)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .context("truncated DOS header")?;
    let signature_end = lfanew.checked_add(4).context("bad PE header offset")?;
    if bytes.get(lfanew..signature_end) != Some(b"PE\0\0".as_slice()) {
        bail!("not a PE image (no PE signature at {lfanew:#x})");
    }
    let coff = signature_end;
    let optional_size = u16_at(coff + 16).context("truncated COFF header")?;
    let optional = coff + 20;
    let magic = u16_at(optional).context("truncated optional header")?;
    if magic != 0x10b && magic != 0x20b {
        bail!("unknown PE optional-header magic {magic:#x}");
    }
    if usize::from(optional_size) < 70 {
        bail!("the PE optional header ({optional_size} bytes) is too short to hold the subsystem");
    }
    u16_at(optional + 68).context("truncated optional header")
}

/// Checks that the staged Windows exe at `binary` is a GUI-subsystem program, so the shipped game
/// opens no console window. A console exe warns; with `enforce` (named build profiles) it fails.
///
/// # Errors
///
/// The binary cannot be read or is not a PE image, or (when `enforce`) it is not a GUI program.
pub fn check_windows_subsystem(binary: &Path, enforce: bool) -> Result<()> {
    let bytes = std::fs::read(binary).with_context(|| format!("reading {}", binary.display()))?;
    let subsystem =
        pe_subsystem(&bytes).with_context(|| format!("reading the PE header of {}", binary.display()))?;
    if subsystem == PE_SUBSYSTEM_WINDOWS_GUI {
        return Ok(());
    }
    let kind = if subsystem == PE_SUBSYSTEM_WINDOWS_CUI {
        "a console program".to_owned()
    } else {
        format!("PE subsystem {subsystem}")
    };
    let why = format!(
        "the shipping exe {} is {kind}: Windows opens a console window beside the game. Add \
         `#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = \"windows\")]` at the top \
         of the launcher's src/main.rs (new projects have it)",
        binary.display()
    );
    if enforce {
        bail!(why);
    }
    tracing::warn!("{why}");
    Ok(())
}

/// The PE resource type of one icon image (`RT_ICON`).
pub const RT_ICON: u32 = 3;

/// The PE resource type of an icon group, the directory Explorer reads (`RT_GROUP_ICON`).
pub const RT_GROUP_ICON: u32 = 14;

/// The PE resource type of the version info (`RT_VERSION`, a `VS_VERSIONINFO` block).
pub const RT_VERSION: u32 = 16;

/// How many resource directory entries [`pe_resources`] visits before it refuses the image (a
/// hostile file can point many entries at one subdirectory).
const MAX_RESOURCE_ENTRIES: usize = 16_384;

/// How many resource data bytes [`pe_resources`] copies before it refuses the image (real game
/// exes carry well under a megabyte of icons and version info).
const MAX_RESOURCE_BYTES: usize = 64 << 20;

/// One resource of a PE image: its type, its numeric name (`None` for a string name) and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeResource {
    /// `RT_*` type id ([`RT_ICON`], [`RT_GROUP_ICON`], [`RT_VERSION`], ...); string-named types
    /// are skipped.
    pub kind: u32,
    /// The numeric name id, `None` when the resource is named by a string.
    pub id: Option<u32>,
    /// The resource data.
    pub data: Vec<u8>,
}

/// The resources of a PE image, read from the resource data directory (index 2) of the optional
/// header (Microsoft's published "PE Format" specification, "The .rsrc Section"): three levels of
/// directory tables (type, name, language), each a 16-byte header with the named and the id entry
/// counts, then 8-byte entries whose offset (relative to the resource section) has the high bit
/// set for a subdirectory; a leaf is a 16-byte data entry holding the data's RVA and size. RVAs map
/// to file offsets through the section table. An image with no resource directory has no
/// resources. Every offset is bounds-checked, so a truncated or hostile image is an error, never
/// a panic.
///
/// # Errors
///
/// `bytes` is not a PE image, or its headers or resource tables are truncated or point outside it.
pub fn pe_resources(bytes: &[u8]) -> Result<Vec<PeResource>> {
    let u16_at = |at: usize| -> Option<u16> {
        bytes
            .get(at..at.checked_add(2)?)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |at: usize| -> Option<u32> {
        bytes
            .get(at..at.checked_add(4)?)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    // The header walk of `pe_subsystem` validates MZ, the signature and the magic.
    pe_subsystem(bytes)?;
    let lfanew = u32_at(0x3c).context("truncated DOS header")? as usize;
    let coff = lfanew + 4;
    let sections = usize::from(u16_at(coff + 2).context("truncated COFF header")?);
    let optional_size = usize::from(u16_at(coff + 16).context("truncated COFF header")?);
    let optional = coff + 20;
    let magic = u16_at(optional).context("truncated optional header")?;
    let (count_at, dirs_at) = if magic == 0x20b { (108, 112) } else { (92, 96) };
    let directories = u32_at(optional + count_at).context("truncated optional header")?;
    // The resource table is data directory 2; it must lie inside the optional header.
    if directories <= 2 || dirs_at + 3 * 8 > optional_size {
        return Ok(Vec::new());
    }
    let rsrc_rva = u32_at(optional + dirs_at + 16).context("truncated data directories")?;
    let rsrc_size = u32_at(optional + dirs_at + 20).context("truncated data directories")?;
    if rsrc_rva == 0 || rsrc_size == 0 {
        return Ok(Vec::new());
    }

    // Section table: name[8], VirtualSize, VirtualAddress, SizeOfRawData, PointerToRawData, ...
    let table = optional + optional_size;
    let mut spans = Vec::with_capacity(sections);
    for i in 0..sections {
        let at = table + i * 40;
        let virtual_size = u32_at(at + 8).context("truncated section table")?;
        let virtual_address = u32_at(at + 12).context("truncated section table")?;
        let raw_size = u32_at(at + 16).context("truncated section table")?;
        let raw_pointer = u32_at(at + 20).context("truncated section table")?;
        spans.push((virtual_address, virtual_size.max(raw_size), raw_pointer, raw_size));
    }
    // The file bytes of `len` bytes at `rva`, when one section holds all of them on disk.
    let at_rva = |rva: u32, len: u32| -> Option<&[u8]> {
        spans.iter().find_map(|&(va, vsize, pointer, raw)| {
            let offset = rva.checked_sub(va)?;
            let end = offset.checked_add(len)?;
            if end > vsize || end > raw {
                return None;
            }
            let start = usize::try_from(pointer.checked_add(offset)?).ok()?;
            bytes.get(start..start.checked_add(len as usize)?)
        })
    };
    // The resource section from the directory's start (offsets in the tables are relative to it).
    let rsrc = spans
        .iter()
        .find_map(|&(va, vsize, pointer, raw)| {
            let offset = rsrc_rva.checked_sub(va)?;
            if offset >= vsize.min(raw) {
                return None;
            }
            let start = usize::try_from(pointer.checked_add(offset)?).ok()?;
            let end = usize::try_from(pointer.checked_add(raw)?).ok()?;
            bytes.get(start..end.min(bytes.len()))
        })
        .context("the resource directory is outside every section")?;
    let r16 = |at: usize| -> Option<u16> {
        rsrc.get(at..at.checked_add(2)?)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let r32 = |at: usize| -> Option<u32> {
        rsrc.get(at..at.checked_add(4)?)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    // The entries of the directory table at `at`: (numeric id or None, offset word).
    let mut visited = 0usize;
    let mut entries = |at: usize| -> Result<Vec<(Option<u32>, u32)>> {
        let named = usize::from(r16(at + 12).context("truncated resource directory")?);
        let ids = usize::from(r16(at + 14).context("truncated resource directory")?);
        visited += named + ids;
        if visited > MAX_RESOURCE_ENTRIES {
            bail!("the resource directory has more than {MAX_RESOURCE_ENTRIES} entries");
        }
        (0..named + ids)
            .map(|i| {
                let e = at + 16 + i * 8;
                let name = r32(e).context("truncated resource directory entry")?;
                let offset = r32(e + 4).context("truncated resource directory entry")?;
                Ok((((name & 0x8000_0000) == 0).then_some(name), offset))
            })
            .collect()
    };
    const SUBDIRECTORY: u32 = 0x8000_0000;
    let mut found = Vec::new();
    let mut copied = 0usize;
    for (kind, type_offset) in entries(0)? {
        let Some(kind) = kind else { continue };
        if type_offset & SUBDIRECTORY == 0 {
            bail!("resource type {kind} is not a directory");
        }
        for (id, name_offset) in entries((type_offset & !SUBDIRECTORY) as usize)? {
            if name_offset & SUBDIRECTORY == 0 {
                bail!("resource {kind}/{id:?} is not a directory");
            }
            for (_, leaf) in entries((name_offset & !SUBDIRECTORY) as usize)? {
                if leaf & SUBDIRECTORY != 0 {
                    bail!("resource {kind}/{id:?} nests deeper than three levels");
                }
                let leaf = leaf as usize;
                let rva = r32(leaf).context("truncated resource data entry")?;
                let size = r32(leaf + 4).context("truncated resource data entry")?;
                let data = at_rva(rva, size).with_context(|| {
                    format!("resource {kind}/{id:?}: {size} bytes at RVA {rva:#x} are outside the file")
                })?;
                copied += data.len();
                if copied > MAX_RESOURCE_BYTES {
                    bail!("the resources hold more than {MAX_RESOURCE_BYTES} bytes");
                }
                found.push(PeResource {
                    kind,
                    id,
                    data: data.to_vec(),
                });
            }
        }
    }
    Ok(found)
}

/// The version info of a PE image, from its `VS_VERSIONINFO` resource ([`parse_version_info`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeVersionInfo {
    /// `VS_FIXEDFILEINFO.dwFileVersionMS/LS` as four 16-bit parts, when present.
    pub fixed_file_version: Option<[u16; 4]>,
    /// The `StringFileInfo` values (`FileVersion`, `ProductName`, `CompanyName`, ...) of the first
    /// string table, in file order.
    pub strings: Vec<(String, String)>,
}

impl PeVersionInfo {
    /// The string value named `key` (`FileVersion`, `ProductVersion`, `ProductName`, ...).
    #[must_use]
    pub fn string(&self, key: &str) -> Option<&str> {
        self.strings
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// One node of a version-info tree: `wLength`, `wValueLength`, `wType`, a NUL-terminated UTF-16
/// key, padding to 32 bits, the value, padding, then child nodes up to `wLength`.
struct VersionNode<'a> {
    key: String,
    value: &'a [u8],
    text: bool,
    children: std::ops::Range<usize>,
}

/// Reads the node at `at` of `data` (offsets and alignment are relative to the resource start).
fn version_node(data: &[u8], at: usize) -> Result<VersionNode<'_>> {
    let u16_at = |at: usize| -> Option<u16> {
        data.get(at..at.checked_add(2)?)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let length = usize::from(u16_at(at).context("truncated version block")?);
    let value_length = usize::from(u16_at(at + 2).context("truncated version block")?);
    let kind = u16_at(at + 4).context("truncated version block")?;
    let end = at + length;
    if length < 6 || end > data.len() {
        bail!("version block at {at:#x} of {length} bytes runs past the resource");
    }
    let mut key = Vec::new();
    let mut cursor = at + 6;
    loop {
        let unit = u16_at(cursor)
            .filter(|_| cursor + 2 <= end)
            .context("unterminated version block key")?;
        cursor += 2;
        if unit == 0 {
            break;
        }
        key.push(unit);
    }
    let align = |n: usize| n.next_multiple_of(4);
    let value_start = align(cursor).min(end);
    // A text value's length counts UTF-16 units, a binary one bytes.
    let text = kind == 1;
    let value_bytes = if text { value_length * 2 } else { value_length };
    let value_end = (value_start + value_bytes).min(end);
    Ok(VersionNode {
        key: String::from_utf16_lossy(&key),
        value: &data[value_start..value_end],
        text,
        children: align(value_end).min(end)..end,
    })
}

/// The child nodes of `node` (at most 256, enough for any real string table).
fn version_children<'a>(data: &'a [u8], node: &VersionNode<'_>) -> Result<Vec<VersionNode<'a>>> {
    let mut out = Vec::new();
    let mut at = node.children.start;
    while at + 6 <= node.children.end && out.len() < 256 {
        // Zero padding after the last child ends the list.
        let length = data
            .get(at..at + 2)
            .map_or(0, |b| usize::from(u16::from_le_bytes([b[0], b[1]])));
        if length == 0 {
            break;
        }
        let child = version_node(data, at)?;
        if at + length > node.children.end {
            bail!("version block {} runs past its parent", child.key);
        }
        out.push(child);
        at = (at + length).next_multiple_of(4);
    }
    Ok(out)
}

/// Parses a `VS_VERSIONINFO` resource (Microsoft's "VS_VERSIONINFO structure" reference): the
/// root's value is the 52-byte `VS_FIXEDFILEINFO` (signature `0xFEEF04BD`, then the struct version
/// and the file version's high and low words), its `StringFileInfo` child holds string tables
/// whose children are the named strings.
///
/// # Errors
///
/// The block is truncated, its lengths run past the resource, or the root is not `VS_VERSION_INFO`.
pub fn parse_version_info(data: &[u8]) -> Result<PeVersionInfo> {
    let root = version_node(data, 0)?;
    if root.key != "VS_VERSION_INFO" {
        bail!(
            "the version resource's root is `{}`, not VS_VERSION_INFO",
            root.key
        );
    }
    let mut info = PeVersionInfo::default();
    let fixed = root.value;
    if fixed.len() >= 16 && fixed[..4] == 0xFEEF_04BDu32.to_le_bytes() {
        let word = |at: usize| u32::from_le_bytes([fixed[at], fixed[at + 1], fixed[at + 2], fixed[at + 3]]);
        let (ms, ls) = (word(8), word(12));
        info.fixed_file_version = Some([(ms >> 16) as u16, ms as u16, (ls >> 16) as u16, ls as u16]);
    }
    for child in version_children(data, &root)? {
        if child.key != "StringFileInfo" {
            continue;
        }
        let Some(table) = version_children(data, &child)?.into_iter().next() else {
            continue;
        };
        for string in version_children(data, &table)? {
            let units: Vec<u16> = string
                .value
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b))
                .take_while(|&u| u != 0)
                .collect();
            let value = if string.text {
                String::from_utf16_lossy(&units)
            } else {
                String::new()
            };
            info.strings.push((string.key, value));
        }
    }
    Ok(info)
}

/// The numeric `major.minor.patch[.build]` of a version string, padded to four parts; a
/// pre-release or build suffix (`-beta`, `+git`) is dropped. `None` when a part is not a 16-bit
/// number.
#[must_use]
pub fn version_parts(version: &str) -> Option<[u16; 4]> {
    let core = version.trim().split(['-', '+']).next().unwrap_or_default();
    let mut parts = [0u16; 4];
    for (count, part) in core.split('.').enumerate() {
        *parts.get_mut(count)? = part.trim().parse().ok()?;
    }
    Some(parts)
}

/// The problems of a Windows exe's resources against the package `version`: a missing icon group
/// or icon image, missing or unreadable version info, or a `FileVersion` (or fixed file version)
/// that differs. Empty when the icon and the version info are in place.
///
/// `version` is `None` when `Regia.toml` sets no `[project] version`: the templates' `build.rs`
/// then embeds the game crate's `CARGO_PKG_VERSION`, so no specific version can mismatch (the
/// icon, presence and parse checks still run).
#[must_use]
pub fn windows_resources_findings(resources: &[PeResource], version: Option<&str>) -> Vec<String> {
    let mut findings = Vec::new();
    let icons: Vec<Option<u32>> = resources
        .iter()
        .filter(|r| r.kind == RT_ICON)
        .map(|r| r.id)
        .collect();
    let groups: Vec<&PeResource> = resources.iter().filter(|r| r.kind == RT_GROUP_ICON).collect();
    if groups.is_empty() {
        findings.push("no icon group (RT_GROUP_ICON): Explorer shows the default program icon".to_owned());
    } else if icons.is_empty() {
        findings.push("an icon group but no icon images (RT_ICON)".to_owned());
    }
    // Each GRPICONDIRENTRY (14 bytes after the 6-byte header) names an RT_ICON id in its last word.
    for group in &groups {
        let count = group
            .data
            .get(4..6)
            .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
        for i in 0..usize::from(count) {
            let at = 6 + i * 14 + 12;
            match group.data.get(at..at + 2) {
                Some(b) => {
                    let id = u32::from(u16::from_le_bytes([b[0], b[1]]));
                    if !icons.contains(&Some(id)) {
                        findings.push(format!(
                            "the icon group names icon image {id}, which the exe lacks"
                        ));
                    }
                }
                None => {
                    findings.push("the icon group (RT_GROUP_ICON) is truncated".to_owned());
                    break;
                }
            }
        }
    }
    let Some(version_resource) = resources.iter().find(|r| r.kind == RT_VERSION) else {
        findings.push("no version info (RT_VERSION): Properties > Details shows no file version".to_owned());
        return findings;
    };
    let info = match parse_version_info(&version_resource.data) {
        Ok(info) => info,
        Err(err) => {
            findings.push(format!("the version info does not parse: {err:#}"));
            return findings;
        }
    };
    let Some(version) = version else {
        return findings;
    };
    let expected = version_parts(version);
    match info.string("FileVersion") {
        None => findings.push("the version info has no FileVersion string".to_owned()),
        Some(found) => {
            let same =
                found.trim() == version.trim() || (expected.is_some() && version_parts(found) == expected);
            if !same {
                findings.push(format!(
                    "FileVersion is `{found}` but the package version is `{version}`"
                ));
            }
        }
    }
    if let (Some(expected), Some(fixed)) = (expected, info.fixed_file_version)
        && fixed != expected
    {
        findings.push(format!(
            "the fixed file version is {}.{}.{}.{} but the package version is `{version}`",
            fixed[0], fixed[1], fixed[2], fixed[3]
        ));
    }
    findings
}

/// Checks that the staged Windows exe at `binary` carries an icon (`RT_GROUP_ICON` with its
/// `RT_ICON` images) and version info (`RT_VERSION`) whose `FileVersion` is `version`, the
/// package version from `Regia.toml` (the game's `build.rs` embeds them, ADR-2645); `version` is
/// `None` when `Regia.toml` sets none (the exe then carries `CARGO_PKG_VERSION`, ADR-2645's
/// template fallback, which nothing can mismatch). A missing or wrong resource warns; with
/// `enforce` (named shipping profiles) it fails.
///
/// # Errors
///
/// The binary cannot be read or is not a PE image, or (when `enforce`) a resource is missing or
/// its version differs.
pub fn check_windows_resources(binary: &Path, version: Option<&str>, enforce: bool) -> Result<()> {
    let bytes = std::fs::read(binary).with_context(|| format!("reading {}", binary.display()))?;
    let findings = match pe_resources(&bytes) {
        Ok(resources) => windows_resources_findings(&resources, version),
        Err(err) => vec![format!("the resource directory does not parse: {err:#}")],
    };
    if findings.is_empty() {
        tracing::info!(exe = %binary.display(), version, "Windows icon and version info present");
        return Ok(());
    }
    let why = format!(
        "the Windows exe {} lacks its resources: {}. New projects embed them from launcher/build.rs \
         (`regia upgrade-project --write` adds it to older ones); the build needs the Windows SDK's \
         rc.exe, or llvm-rc when cross-compiling (book: Packaging, Windows icon and version)",
        binary.display(),
        findings.join("; ")
    );
    if enforce {
        bail!(why);
    }
    tracing::warn!("{why}");
    Ok(())
}

/// One pak of a [`CookedPackReport`].
#[derive(Debug, Clone, Serialize)]
pub struct CookedPakReport {
    /// File name of the pak.
    pub file: String,
    /// What its entries are.
    pub summary: PakSummary,
    /// The cooked-pack rules it breaks, errors first.
    pub findings: Vec<CookedPackFinding>,
}

/// The cooked-pack report over a set of paks (`regia cook --ship`, `--pack-report`, and a shipping
/// `regia package`; ADR-1594). Written as `cooked_pack.json` next to the paks.
#[derive(Debug, Clone, Serialize)]
pub struct CookedPackReport {
    /// Platform the paks were checked for.
    pub platform: String,
    /// One entry per pak, in path order.
    pub paks: Vec<CookedPakReport>,
    /// Error findings over all paks.
    pub errors: usize,
    /// Warning findings over all paks.
    pub warnings: usize,
}

impl CookedPackReport {
    /// Human-readable lines: one per pak, then one per finding.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for pak in &self.paks {
            let s = &pak.summary;
            let families: Vec<String> = s
                .texture_families
                .iter()
                .map(|(f, n)| format!("{n} {f}"))
                .collect();
            out.push_str(&format!(
                "cooked pack {} ({}): {} entries; meshes {} gpu_ready, {} compact; textures {} ({}); {} rgba8 images\n",
                pak.file,
                self.platform,
                s.entries,
                s.meshes_of(cooked_pack::MeshForm::GpuReady),
                s.meshes_of(cooked_pack::MeshForm::Compact),
                s.textures,
                if families.is_empty() {
                    "not inspected".to_owned()
                } else {
                    families.join(", ")
                },
                s.images_rgba8,
            ));
            for f in &pak.findings {
                let level = match f.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warn",
                };
                out.push_str(&format!("  {level} {} ({}): {}\n", f.code, f.count, f.message));
                if !f.examples.is_empty() {
                    out.push_str(&format!("    e.g. {}\n", f.examples.join(", ")));
                }
            }
        }
        out
    }
}

/// Inspects every pak in `paks` (each texture's KTX2 header included) against `platform`'s
/// cooked-pack rules ([`regia_asset::cooked_pack::check_pak_summary`]).
///
/// # Errors
///
/// A pak does not open.
pub fn cooked_pack_report(paks: &[PathBuf], platform: &str) -> Result<CookedPackReport> {
    let mut out = CookedPackReport {
        platform: platform.to_owned(),
        paks: Vec::with_capacity(paks.len()),
        errors: 0,
        warnings: 0,
    };
    let mut sorted = paks.to_vec();
    sorted.sort();
    for path in &sorted {
        let reader = std::sync::Arc::new(
            regia_asset::PakReader::open(path).with_context(|| format!("opening {}", path.display()))?,
        );
        let name = path
            .file_stem()
            .map_or_else(|| "pak".to_owned(), |s| s.to_string_lossy().into_owned());
        let summary = cooked_pack::summarize_pak(&reader, &name, platform, true);
        let findings = cooked_pack::check_pak_summary(&summary, platform);
        for f in &findings {
            match f.severity {
                Severity::Error => out.errors += 1,
                Severity::Warning => out.warnings += 1,
            }
        }
        out.paks.push(CookedPakReport {
            file: path
                .file_name()
                .map_or_else(|| name.clone(), |s| s.to_string_lossy().into_owned()),
            summary,
            findings,
        });
    }
    Ok(out)
}

/// The `.fpak` files directly inside `dir`.
///
/// # Errors
///
/// `dir` cannot be read.
pub fn paks_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "fpak") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// [`cooked_pack_report`] that fails on an error finding (warnings are logged).
///
/// # Errors
///
/// A pak does not open, or a pak breaks a cooked-pack rule of [`Severity::Error`].
pub fn check_cooked_paks(paks: &[PathBuf], platform: &str) -> Result<CookedPackReport> {
    let report = cooked_pack_report(paks, platform)?;
    if report.errors > 0 {
        let text = report.to_text();
        bail!(
            "the paks fail the cooked-pack check (ADR-1594):\n{}",
            text.trim_end()
        );
    }
    if report.warnings > 0 {
        let text = report.to_text();
        tracing::warn!("cooked-pack check:\n{}", text.trim_end());
    }
    Ok(report)
}

/// One lightmap entry past the seam budget.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LightmapSeamOver {
    /// The lightmap's manifest path in the pak.
    pub lightmap: String,
    /// The entry's name.
    pub entry: String,
    /// The entry's seam figures.
    pub seams: regia_asset::lightmap::LightmapSeams,
}

/// What the seam budget saw in a set of paks.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LightmapSeamReport {
    /// Lightmaps found.
    pub lightmaps: usize,
    /// Entries with seam figures.
    pub measured: usize,
    /// Entries with none: a file from before format version 3 (re-bake it to measure it), or an
    /// entry with no seam.
    pub unmeasured: usize,
    /// Entries past the budget.
    pub over: Vec<LightmapSeamOver>,
}

/// Reads the seam figures of every `.flmap` in `paks` from its header (no atlas is decoded).
///
/// # Errors
///
/// A pak does not open, or a lightmap in it does not read.
pub fn lightmap_seam_report(paks: &[PathBuf], budget: f32) -> Result<LightmapSeamReport> {
    let mut report = LightmapSeamReport::default();
    let mut sorted = paks.to_vec();
    sorted.sort();
    for path in &sorted {
        let reader =
            regia_asset::PakReader::open(path).with_context(|| format!("opening {}", path.display()))?;
        for item in &reader.toc.manifest {
            if !item.path.ends_with(".flmap") {
                continue;
            }
            let file = reader
                .read_decompressed(item.id)
                .with_context(|| format!("reading {} in {}", item.path, path.display()))?;
            let (_, payload) = regia_asset::decode_cooked_file(&file)
                .with_context(|| format!("decoding {} in {}", item.path, path.display()))?;
            let header = regia_asset::lightmap::Lightmap::read_header(payload)
                .with_context(|| format!("reading {} in {}", item.path, path.display()))?;
            report.lightmaps += 1;
            for entry in &header.entries {
                match entry.seams {
                    None => report.unmeasured += 1,
                    Some(seams) => {
                        report.measured += 1;
                        if seams.p95 > budget {
                            report.over.push(LightmapSeamOver {
                                lightmap: item.path.clone(),
                                entry: entry.name.clone(),
                                seams,
                            });
                        }
                    }
                }
            }
        }
    }
    Ok(report)
}

/// [`lightmap_seam_report`] that fails when an entry passes `budget` (`[lightmaps] seam_budget`).
///
/// # Errors
///
/// `lightmap_seam_budget`, naming the entries; or the report's errors.
pub fn check_lightmap_seams(paks: &[PathBuf], budget: f32) -> Result<LightmapSeamReport> {
    let report = lightmap_seam_report(paks, budget)?;
    if !report.over.is_empty() {
        let list: Vec<String> = report
            .over
            .iter()
            .map(|o| {
                format!(
                    "{} {}: p95 {:.3}, max {:.3}, worst chart {}",
                    o.lightmap, o.entry, o.seams.p95, o.seams.max, o.seams.worst_chart
                )
            })
            .collect();
        bail!(
            "lightmap_seam_budget: {} lightmap entries pass the chart-seam budget {budget} ([lightmaps] seam_budget, ADR-2024). Re-bake with more --samples or raise the budget:\n{}",
            report.over.len(),
            list.join("\n")
        );
    }
    if report.unmeasured > 0 {
        tracing::warn!(
            "{} lightmap entries have no seam figures (re-bake them to measure them)",
            report.unmeasured
        );
    }
    Ok(report)
}

/// The engine's umbrella crate in a `[dependencies]` table: the entry named `regia`, or one that
/// renames it (`<key> = { package = "regia", ... }`, which keeps a project from before the rename
/// to Regia Engine compiling, ADR-1429). Returns the dependency's key and value.
pub(crate) fn engine_dependency(deps: &toml::Value) -> Option<(&str, &toml::Value)> {
    let main = regia_core::brand::MAIN_CRATE;
    let table = deps.as_table()?;
    if let Some(dep) = table.get(main) {
        return Some((main, dep));
    }
    table
        .iter()
        .find(|(_, dep)| dep.get("package").and_then(toml::Value::as_str) == Some(main))
        .map(|(key, dep)| (key.as_str(), dep))
}

/// Whether `rel` (a path inside a package, `/`-separated) is editor-only content: anything under a
/// root-level `.regia/`, `editor/` or `assets/editor/` directory, an editor autosave/session
/// file, or a level directory (`*.level.d/`, the authoring-only per-entity form of spec 37 § 3.6).
/// A game's own `assets/ui/editor/...` is not editor content.
pub fn is_editor_content(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let in_editor_dir = matches!(
        parts.as_slice(),
        [".regia" | "editor", _, ..] | ["assets", "editor", _, ..]
    ) || matches!(
        parts.as_slice(),
        [dir, _, ..] if *dir == regia_core::brand::LEGACY_PROJECT_STATE_DIR
    );
    if in_editor_dir {
        return true;
    }
    for (i, part) in parts.iter().enumerate() {
        let is_last = i + 1 == parts.len();
        if part.ends_with(regia_scene::level_dir::LEVEL_DIR_SUFFIX) {
            return true;
        }
        if is_last
            && (part.ends_with(".editor.ron")
                || part.ends_with(".autosave.ron")
                || part.ends_with(".autosave"))
        {
            return true;
        }
    }
    false
}

/// Fails when the staged package under `root` holds a `.fpak` that is not one of `expected_paks`
/// directly inside `content_dir` (a stray pak: an old chunk, another platform's or profile's pak,
/// DLC), or any editor-only content ([`is_editor_content`]).
///
/// # Errors
///
/// The tree cannot be read, or it contains a stray pak or editor content.
pub fn check_package_tree(root: &Path, content_dir: &Path, expected_paks: &[String]) -> Result<()> {
    let mut stray = Vec::new();
    let mut editor = Vec::new();
    for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel_text = rel.to_string_lossy().replace('\\', "/");
        if !rel_text.is_empty() && is_editor_content(&rel_text) {
            editor.push(rel_text.clone());
        }
        if entry.file_type().is_file() && entry.path().extension().is_some_and(|e| e == "fpak") {
            let expected = entry.path().parent() == Some(content_dir)
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| expected_paks.iter().any(|e| e == n));
            if !expected {
                stray.push(rel_text);
            }
        }
    }
    if stray.is_empty() && editor.is_empty() {
        return Ok(());
    }
    let mut msg = String::from("the package fails ship_check:");
    if !stray.is_empty() {
        msg.push_str(&format!(
            "\n  stray pak(s) (expected only {} in {}): {}",
            expected_paks.join(", "),
            content_dir.strip_prefix(root).unwrap_or(content_dir).display(),
            stray.join(", ")
        ));
    }
    if !editor.is_empty() {
        msg.push_str(&format!("\n  editor content: {}", editor.join(", ")));
    }
    bail!(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts `WARN` events; a minimal `tracing` subscriber so the test needs no extra
    /// dependency (same pattern as the pipeline's `gltf_extensions` tests).
    struct WarnCounter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl tracing::Subscriber for WarnCounter {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    fn tracing_subscriber(counter: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> WarnCounter {
        WarnCounter(counter)
    }

    /// A minimal PE image: a DOS header pointing at `lfanew`, the signature, a COFF header and an
    /// optional header of `magic` with `subsystem` at offset 68.
    fn synthetic_pe(lfanew: usize, magic: u16, subsystem: u16) -> Vec<u8> {
        let optional_size: u16 = if magic == 0x20b { 240 } else { 224 };
        let mut b = vec![0u8; lfanew + 24 + usize::from(optional_size)];
        b[..2].copy_from_slice(b"MZ");
        b[0x3c..0x40].copy_from_slice(&(lfanew as u32).to_le_bytes());
        b[lfanew..lfanew + 4].copy_from_slice(b"PE\0\0");
        let coff = lfanew + 4;
        b[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes());
        b[coff + 16..coff + 18].copy_from_slice(&optional_size.to_le_bytes());
        let optional = coff + 20;
        b[optional..optional + 2].copy_from_slice(&magic.to_le_bytes());
        b[optional + 68..optional + 70].copy_from_slice(&subsystem.to_le_bytes());
        b
    }

    #[test]
    fn pe_subsystem_reads_pe32_and_pe32_plus() {
        let gui = synthetic_pe(0x80, 0x20b, PE_SUBSYSTEM_WINDOWS_GUI);
        assert_eq!(pe_subsystem(&gui).expect("PE32+"), PE_SUBSYSTEM_WINDOWS_GUI);
        let console = synthetic_pe(0xf8, 0x10b, PE_SUBSYSTEM_WINDOWS_CUI);
        assert_eq!(pe_subsystem(&console).expect("PE32"), PE_SUBSYSTEM_WINDOWS_CUI);
    }

    #[test]
    fn pe_subsystem_refuses_what_is_not_a_pe_image() {
        let good = synthetic_pe(0x80, 0x20b, PE_SUBSYSTEM_WINDOWS_GUI);
        assert!(pe_subsystem(b"\x7fELF").is_err(), "an ELF file");
        assert!(pe_subsystem(&good[..0x3e]).is_err(), "truncated DOS header");
        let mut no_signature = good.clone();
        no_signature[0x80] = b'X';
        assert!(pe_subsystem(&no_signature).is_err());
        let mut far = good.clone();
        far[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(pe_subsystem(&far).is_err(), "e_lfanew past the end");
        let mut bad_magic = good.clone();
        bad_magic[0x80 + 24] = 0x07;
        assert!(pe_subsystem(&bad_magic).is_err());
        let mut short_optional = good.clone();
        short_optional[0x80 + 20..0x80 + 22].copy_from_slice(&64u16.to_le_bytes());
        assert!(pe_subsystem(&short_optional).is_err());
        assert!(
            pe_subsystem(&good[..0x80 + 24 + 69]).is_err(),
            "cut inside the subsystem field"
        );
    }

    #[test]
    fn a_console_exe_warns_on_plain_shipping_and_fails_a_named_profile() {
        let dir = regia_test_utils::unique_temp_dir("cli-pe-subsystem");
        let console = dir.join("console.exe");
        let gui = dir.join("gui.exe");
        std::fs::write(&console, synthetic_pe(0x80, 0x20b, PE_SUBSYSTEM_WINDOWS_CUI)).expect("write");
        std::fs::write(&gui, synthetic_pe(0x80, 0x20b, PE_SUBSYSTEM_WINDOWS_GUI)).expect("write");
        assert!(check_windows_subsystem(&gui, true).is_ok());
        assert!(
            check_windows_subsystem(&console, false).is_ok(),
            "plain shipping only warns"
        );
        let error = check_windows_subsystem(&console, true).expect_err("a named profile enforces");
        assert!(format!("{error:#}").contains("windows_subsystem"), "{error:#}");
        assert!(check_windows_subsystem(&dir.join("missing.exe"), false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pads `b` with zeros to a multiple of 4 bytes.
    fn pad4(b: &mut Vec<u8>) {
        b.resize(b.len().next_multiple_of(4), 0);
    }

    /// One version-info node: header, NUL-terminated UTF-16 key, padding, value, then children
    /// (each started on a 32-bit boundary).
    fn version_block(key: &str, value: &[u8], text: bool, children: &[Vec<u8>]) -> Vec<u8> {
        let mut b = vec![0u8; 6];
        for unit in key.encode_utf16().chain([0]) {
            b.extend_from_slice(&unit.to_le_bytes());
        }
        pad4(&mut b);
        b.extend_from_slice(value);
        for child in children {
            pad4(&mut b);
            b.extend_from_slice(child);
        }
        let value_length = if text { value.len() / 2 } else { value.len() };
        let length = b.len() as u16;
        b[0..2].copy_from_slice(&length.to_le_bytes());
        b[2..4].copy_from_slice(&(value_length as u16).to_le_bytes());
        b[4..6].copy_from_slice(&u16::from(text).to_le_bytes());
        b
    }

    /// A text value: UTF-16 with its NUL.
    fn utf16z(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    /// A `VS_VERSIONINFO` with `file_version` as the FileVersion string and `fixed` as the binary
    /// file version, laid out as a resource compiler writes it.
    fn version_resource(file_version: &str, fixed: [u16; 4]) -> Vec<u8> {
        let mut info = Vec::new();
        let ms = (u32::from(fixed[0]) << 16) | u32::from(fixed[1]);
        let ls = (u32::from(fixed[2]) << 16) | u32::from(fixed[3]);
        for word in [
            0xFEEF_04BD,
            0x0001_0000,
            ms,
            ls,
            ms,
            ls,
            0x3f,
            0,
            0x0004_0004,
            1,
            0,
            0,
            0,
        ] {
            info.extend_from_slice(&u32::to_le_bytes(word));
        }
        let strings: Vec<Vec<u8>> = [
            ("CompanyName", "Example Studio"),
            ("FileDescription", "Night Train"),
            ("FileVersion", file_version),
            ("ProductName", "Night Train"),
            ("ProductVersion", file_version),
        ]
        .iter()
        .map(|(k, v)| version_block(k, &utf16z(v), true, &[]))
        .collect();
        let table = version_block("000004b0", &[], true, &strings);
        let string_info = version_block("StringFileInfo", &[], true, &[table]);
        let translation = version_block("Translation", &[0, 0, 0xb0, 0x04], false, &[]);
        let var_info = version_block("VarFileInfo", &[], true, &[translation]);
        version_block("VS_VERSION_INFO", &info, false, &[string_info, var_info])
    }

    /// A GRPICONDIR naming icon images `ids` (6-byte header, 14-byte entries).
    fn icon_group(ids: &[u16]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[0, 0, 1, 0]);
        b.extend_from_slice(&(ids.len() as u16).to_le_bytes());
        for &id in ids {
            b.extend_from_slice(&[16, 16, 0, 0, 1, 0, 32, 0]);
            b.extend_from_slice(&8u32.to_le_bytes());
            b.extend_from_slice(&id.to_le_bytes());
        }
        b
    }

    /// The resources of a valid game exe: four icon images, their group, the version info.
    fn game_resources(version: &str) -> Vec<(u32, u32, Vec<u8>)> {
        let mut out: Vec<(u32, u32, Vec<u8>)> = (1..=4)
            .map(|id| (RT_ICON, id, b"\x89PNG\r\n\x1a\n".to_vec()))
            .collect();
        out.push((RT_GROUP_ICON, 1, icon_group(&[1, 2, 3, 4])));
        let fixed = version_parts(version).unwrap_or_default();
        out.push((RT_VERSION, 1, version_resource(version, fixed)));
        out
    }

    /// The `.rsrc` section of `resources` (type, id, data) for a section at `rva`: the three
    /// directory levels (type, name, language 0x409), the data entries, then the data.
    fn resource_section(resources: &[(u32, u32, Vec<u8>)], rva: u32) -> Vec<u8> {
        let mut kinds: Vec<u32> = resources.iter().map(|r| r.0).collect();
        kinds.sort_unstable();
        kinds.dedup();
        let dir_size = |n: usize| 16 + 8 * n;
        // Layout: root, one name directory per type, one language directory per resource, data
        // entries, data.
        let root_size = dir_size(kinds.len());
        let names_at: Vec<usize> = {
            let mut at = root_size;
            kinds
                .iter()
                .map(|k| {
                    let here = at;
                    at += dir_size(resources.iter().filter(|r| r.0 == *k).count());
                    here
                })
                .collect()
        };
        let mut at = root_size
            + kinds
                .iter()
                .map(|k| dir_size(resources.iter().filter(|r| r.0 == *k).count()))
                .sum::<usize>();
        let langs_at: Vec<usize> = resources
            .iter()
            .map(|_| {
                let here = at;
                at += dir_size(1);
                here
            })
            .collect();
        let entries_at: Vec<usize> = resources
            .iter()
            .map(|_| {
                let here = at;
                at += 16;
                here
            })
            .collect();
        let data_at: Vec<usize> = resources
            .iter()
            .map(|r| {
                at = at.next_multiple_of(8);
                let here = at;
                at += r.2.len();
                here
            })
            .collect();
        let mut b = vec![0u8; at];
        let put32 = |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let header = |b: &mut Vec<u8>, at: usize, ids: usize| {
            b[at + 14..at + 16].copy_from_slice(&(ids as u16).to_le_bytes());
        };
        header(&mut b, 0, kinds.len());
        for (i, kind) in kinds.iter().enumerate() {
            put32(&mut b, 16 + i * 8, *kind);
            put32(&mut b, 16 + i * 8 + 4, 0x8000_0000 | names_at[i] as u32);
            let members: Vec<usize> = (0..resources.len())
                .filter(|&r| resources[r].0 == *kind)
                .collect();
            header(&mut b, names_at[i], members.len());
            for (j, &r) in members.iter().enumerate() {
                let e = names_at[i] + 16 + j * 8;
                put32(&mut b, e, resources[r].1);
                put32(&mut b, e + 4, 0x8000_0000 | langs_at[r] as u32);
            }
        }
        for (r, resource) in resources.iter().enumerate() {
            header(&mut b, langs_at[r], 1);
            put32(&mut b, langs_at[r] + 16, 0x409);
            put32(&mut b, langs_at[r] + 20, entries_at[r] as u32);
            put32(&mut b, entries_at[r], rva + data_at[r] as u32);
            put32(&mut b, entries_at[r] + 4, resource.2.len() as u32);
            b[data_at[r]..data_at[r] + resource.2.len()].copy_from_slice(&resource.2);
        }
        b
    }

    /// A PE32+ GUI image with one `.rsrc` section (RVA 0x1000, file offset 0x400) holding
    /// `resources`, and the resource data directory pointing at it.
    fn pe_with_resources(resources: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
        const RVA: u32 = 0x1000;
        const RAW: usize = 0x400;
        let mut b = synthetic_pe(0x80, 0x20b, PE_SUBSYSTEM_WINDOWS_GUI);
        let coff = 0x80 + 4;
        let optional = coff + 20;
        let section = resource_section(resources, RVA);
        b[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        b[optional + 108..optional + 112].copy_from_slice(&16u32.to_le_bytes());
        b[optional + 112 + 16..optional + 112 + 20].copy_from_slice(&RVA.to_le_bytes());
        b[optional + 112 + 20..optional + 112 + 24].copy_from_slice(&(section.len() as u32).to_le_bytes());
        let mut header = Vec::new();
        header.extend_from_slice(b".rsrc\0\0\0");
        let raw_size = section.len().next_multiple_of(0x200) as u32;
        for word in [
            section.len() as u32,
            RVA,
            raw_size,
            RAW as u32,
            0,
            0,
            0,
            0x4000_0040,
        ] {
            header.extend_from_slice(&word.to_le_bytes());
        }
        header.truncate(40);
        b.extend_from_slice(&header);
        b.resize(RAW, 0);
        b.extend_from_slice(&section);
        b.resize(RAW + raw_size as usize, 0);
        b
    }

    #[test]
    fn pe_resources_reads_icons_groups_and_version_info() {
        let image = pe_with_resources(&game_resources("1.4.2"));
        let resources = pe_resources(&image).expect("resources");
        let kinds: Vec<(u32, Option<u32>)> = resources.iter().map(|r| (r.kind, r.id)).collect();
        assert_eq!(
            kinds,
            [
                (RT_ICON, Some(1)),
                (RT_ICON, Some(2)),
                (RT_ICON, Some(3)),
                (RT_ICON, Some(4)),
                (RT_GROUP_ICON, Some(1)),
                (RT_VERSION, Some(1)),
            ]
        );
        assert!(resources[0].data.starts_with(b"\x89PNG"));
        let version = resources.iter().find(|r| r.kind == RT_VERSION).expect("version");
        let info = parse_version_info(&version.data).expect("parses");
        assert_eq!(info.fixed_file_version, Some([1, 4, 2, 0]));
        assert_eq!(info.string("FileVersion"), Some("1.4.2"));
        assert_eq!(info.string("ProductName"), Some("Night Train"));
        assert_eq!(info.string("CompanyName"), Some("Example Studio"));
        assert_eq!(info.strings.len(), 5);
        assert!(windows_resources_findings(&resources, Some("1.4.2")).is_empty());
        assert!(
            windows_resources_findings(&resources, Some("1.4.2.0")).is_empty(),
            "four parts name the same version"
        );
    }

    #[test]
    fn an_exe_with_icon_and_matching_version_passes_a_named_profile() {
        let dir = regia_test_utils::unique_temp_dir("cli-pe-resources");
        let exe = dir.join("game.exe");
        std::fs::write(&exe, pe_with_resources(&game_resources("0.3.1"))).expect("write");
        check_windows_resources(&exe, Some("0.3.1"), true).expect("valid resources pass");
        assert!(check_windows_resources(&dir.join("missing.exe"), Some("0.3.1"), false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_icon_warns_on_plain_shipping_and_fails_a_named_profile() {
        let dir = regia_test_utils::unique_temp_dir("cli-pe-no-icon");
        let exe = dir.join("game.exe");
        let only_version: Vec<_> = game_resources("1.0.0")
            .into_iter()
            .filter(|r| r.0 == RT_VERSION)
            .collect();
        std::fs::write(&exe, pe_with_resources(&only_version)).expect("write");
        assert!(
            check_windows_resources(&exe, Some("1.0.0"), false).is_ok(),
            "warns only"
        );
        let err = check_windows_resources(&exe, Some("1.0.0"), true).expect_err("enforced");
        assert!(format!("{err:#}").contains("RT_GROUP_ICON"), "{err:#}");

        // A group without its images, and a group naming an image that is not there.
        let group_only: Vec<_> = game_resources("1.0.0")
            .into_iter()
            .filter(|r| r.0 != RT_ICON)
            .collect();
        let findings = windows_resources_findings(
            &pe_resources(&pe_with_resources(&group_only)).expect("r"),
            Some("1.0.0"),
        );
        assert!(findings.iter().any(|f| f.contains("RT_ICON")), "{findings:?}");
        let mut dangling = game_resources("1.0.0");
        dangling.retain(|r| !(r.0 == RT_ICON && r.1 == 3));
        let findings = windows_resources_findings(
            &pe_resources(&pe_with_resources(&dangling)).expect("r"),
            Some("1.0.0"),
        );
        assert_eq!(
            findings,
            ["the icon group names icon image 3, which the exe lacks"]
        );

        // No resource directory at all (the subsystem fixture): both resources are reported.
        let bare = pe_resources(&synthetic_pe(0x80, 0x20b, PE_SUBSYSTEM_WINDOWS_GUI)).expect("no rsrc");
        assert!(bare.is_empty());
        let findings = windows_resources_findings(&bare, Some("1.0.0"));
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings[1].contains("RT_VERSION"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_file_version_is_reported_and_enforced() {
        let dir = regia_test_utils::unique_temp_dir("cli-pe-version");
        let exe = dir.join("game.exe");
        std::fs::write(&exe, pe_with_resources(&game_resources("0.1.0"))).expect("write");
        assert!(check_windows_resources(&exe, Some("1.2.0"), false).is_ok());
        let err = check_windows_resources(&exe, Some("1.2.0"), true).expect_err("enforced");
        let text = format!("{err:#}");
        assert!(
            text.contains("FileVersion is `0.1.0` but the package version is `1.2.0`"),
            "{text}"
        );
        assert!(text.contains("fixed file version is 0.1.0.0"), "{text}");
    }

    /// corr-low cli: a Regia.toml without `[project] version` means the exe legitimately carries
    /// `CARGO_PKG_VERSION` (the templates' build.rs fallback), so the version comparison is
    /// skipped while the icon and presence checks still run.
    #[test]
    fn with_no_manifest_version_any_exe_version_passes() {
        let dir = regia_test_utils::unique_temp_dir("cli-pe-no-manifest-version");
        let exe = dir.join("game.exe");
        std::fs::write(&exe, pe_with_resources(&game_resources("0.37.0"))).expect("write");
        assert!(
            check_windows_resources(&exe, None, true).is_ok(),
            "no expected version, nothing to mismatch"
        );
        // The same exe against a set version still mismatches: the check itself is alive.
        let err = check_windows_resources(&exe, Some("1.2.0"), true).expect_err("enforced");
        assert!(format!("{err:#}").contains("FileVersion is `0.37.0`"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The documented "no asset database" warning of [`check_graph_layouts`] must actually fire:
    /// `AssetDatabase::open` returns an EMPTY database (not an error) when nothing was cooked.
    #[test]
    fn graph_layouts_of_an_uncooked_project_warn_instead_of_checking_nothing() {
        let dir = regia_test_utils::unique_temp_dir("cli-graph-layouts-no-db");
        std::fs::create_dir_all(&dir).expect("project");
        let warns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = warns.clone();
        let subscriber = tracing_subscriber(counted);
        let result = tracing::subscriber::with_default(subscriber, || {
            check_graph_layouts(&dir, &dir.join("cache"), "linux", false)
        });
        result.expect("a missing database is a warning, not an error");
        assert_eq!(
            warns.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly the 'no asset database' warning"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A malformed `[cook] ship_platforms` is named in a warning, not silently read as "ship
    /// nothing" (corr-low cli).
    #[test]
    fn a_malformed_ship_platforms_key_warns() {
        let warns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shipped = tracing::subscriber::with_default(tracing_subscriber(warns.clone()), || {
            shipped_platforms("[cook]\nship_platforms = [1]\n", "linux")
        });
        assert_eq!(shipped, vec!["linux".to_string()]);
        assert_eq!(
            warns.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the malformed key is named"
        );
    }

    #[test]
    fn a_file_version_string_that_agrees_with_a_different_binary_version() {
        let dir = regia_test_utils::unique_temp_dir("cli-pe-version-string-agrees");
        // A FileVersion string that agrees while the binary version does not.
        let mut resources = game_resources("2.0.0");
        if let Some(v) = resources.iter_mut().find(|r| r.0 == RT_VERSION) {
            v.2 = version_resource("2.0.0", [1, 9, 0, 0]);
        }
        let findings = windows_resources_findings(
            &pe_resources(&pe_with_resources(&resources)).expect("r"),
            Some("2.0.0"),
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].contains("1.9.0.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_parts_pad_and_drop_suffixes() {
        assert_eq!(version_parts("1.4.2"), Some([1, 4, 2, 0]));
        assert_eq!(version_parts("1.4.2.7"), Some([1, 4, 2, 7]));
        assert_eq!(version_parts(" 1.4 "), Some([1, 4, 0, 0]));
        assert_eq!(version_parts("1.4.2-beta.1+git"), Some([1, 4, 2, 0]));
        assert_eq!(version_parts("1.x"), None);
        assert_eq!(version_parts("1.2.3.4.5"), None);
        assert_eq!(version_parts("70000"), None);
    }

    #[test]
    fn truncated_or_hostile_resource_tables_are_errors_not_panics() {
        let good = pe_with_resources(&game_resources("1.4.2"));
        // Every prefix: an error or a (partial) result, never a panic.
        for cut in (0..good.len()).step_by(3) {
            let _ = pe_resources(&good[..cut]);
        }
        assert!(
            pe_resources(&good[..0x400 + 40]).is_err(),
            "cut inside the tables"
        );
        // Every single-byte corruption of the headers and tables.
        for at in 0..0x400 + (good.len() - 0x400).min(0x200) {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut bad = good.clone();
                bad[at] ^= flip;
                if let Ok(resources) = pe_resources(&bad) {
                    let _ = windows_resources_findings(&resources, Some("1.4.2"));
                }
            }
        }
        let rsrc = 0x400;
        // A type entry that points back at the root directory: the walk is bounded.
        let mut loop_back = good.clone();
        loop_back[rsrc + 20..rsrc + 24].copy_from_slice(&0x8000_0000u32.to_le_bytes());
        assert!(pe_resources(&loop_back).is_err());
        // A directory claiming 65535 entries.
        let mut many = good.clone();
        many[rsrc + 14..rsrc + 16].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(pe_resources(&many).is_err());
        // A data entry whose RVA lies outside every section.
        let resources = game_resources("1.4.2");
        let section = resource_section(&resources, 0x1000);
        let mut outside = good.clone();
        let first_data_entry = (0..section.len() / 4)
            .map(|i| i * 4)
            .find(|&i| {
                let v = u32::from_le_bytes([section[i], section[i + 1], section[i + 2], section[i + 3]]);
                v >= 0x1000 && v < 0x1000 + section.len() as u32 && i > 16
            })
            .expect("a data RVA");
        outside[rsrc + first_data_entry..rsrc + first_data_entry + 4]
            .copy_from_slice(&0x7fff_0000u32.to_le_bytes());
        let err = pe_resources(&outside).expect_err("outside the file");
        assert!(format!("{err:#}").contains("outside the file"), "{err:#}");
        // The resource directory's RVA outside every section.
        let mut no_section = good.clone();
        let dir = 0x80 + 24 + 112 + 16;
        no_section[dir..dir + 4].copy_from_slice(&0x0900_0000u32.to_le_bytes());
        assert!(pe_resources(&no_section).is_err());
        // Not a PE image at all.
        assert!(pe_resources(b"MZ").is_err());
        assert!(pe_resources(b"\x7fELF\x02\x01\x01").is_err());
    }

    #[test]
    fn hostile_version_blocks_are_errors_not_panics() {
        let good = version_resource("1.4.2", [1, 4, 2, 0]);
        assert!(parse_version_info(&good).is_ok());
        for cut in 0..good.len() {
            let _ = parse_version_info(&good[..cut]);
        }
        for at in 0..good.len() {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut bad = good.clone();
                bad[at] ^= flip;
                let _ = parse_version_info(&bad);
            }
        }
        let mut long = good.clone();
        long[0..2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(parse_version_info(&long).is_err(), "a length past the resource");
        let unterminated = [12u8, 0, 0, 0, 0, 0, b'V', 0, b'S', 0, b'_', 0];
        assert!(parse_version_info(&unterminated).is_err());
        let other = version_block("NOT_VERSION", &[], false, &[]);
        assert!(parse_version_info(&other).is_err());
        assert!(parse_version_info(&[]).is_err());
    }

    /// The resource reader on a real image (ADR-2645): set `REGIA_WINDOWS_RESOURCES_EXE` to an exe
    /// or DLL linked from the template build script's resources (for example with llvm-rc and
    /// lld-link, or a Windows build) and `REGIA_WINDOWS_RESOURCES_VERSION` to its package version.
    #[test]
    #[ignore = "needs a Windows image; see the doc comment"]
    fn a_real_windows_image_passes_the_resource_check() {
        let Ok(path) = std::env::var("REGIA_WINDOWS_RESOURCES_EXE") else {
            return;
        };
        let version = std::env::var("REGIA_WINDOWS_RESOURCES_VERSION").unwrap_or_else(|_| "0.1.0".to_owned());
        let bytes = std::fs::read(&path).expect("read the image");
        let resources = pe_resources(&bytes).expect("resources parse");
        for r in &resources {
            eprintln!("resource type {} id {:?}: {} bytes", r.kind, r.id, r.data.len());
        }
        if let Some(v) = resources.iter().find(|r| r.kind == RT_VERSION) {
            eprintln!("{:?}", parse_version_info(&v.data).expect("version parses"));
        }
        check_windows_resources(std::path::Path::new(&path), Some(&version), true).expect("check passes");
    }

    const OBJDUMP_T: &str = "\
arena:     file format elf64-x86-64

DYNAMIC SYMBOL TABLE:
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.2.5) free
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.17) clock_gettime
0000000000000000      DF *UND*\t0000000000000000  GLIBC_2.28  statx
0000000000000000  w   D  *UND*\t0000000000000000 (GLIBC_PRIVATE) _dl_find_object
0000000000000000      DF *UND*\t0000000000000000 (ALSA_0.9) snd_pcm_open
";

    /// C-26 (ADR-8835): the glibc floor check reads the version-needed table from the binary's
    /// bytes (no objdump) and names the library that needs the newest version.
    #[test]
    fn check_linux_binary_reads_the_glibc_floor_from_elf_bytes() {
        use crate::commands::elf_versions::fixture::elf_with_needs;
        let dir = regia_test_utils::unique_temp_dir("cli-glibc-floor");
        let old = dir.join("old");
        std::fs::write(
            &old,
            elf_with_needs(&[
                ("libm.so.6", &["GLIBC_2.29"]),
                ("libc.so.6", &["GLIBC_2.2.5", "GLIBC_2.31"]),
            ]),
        )
        .expect("write fixture");
        let report = check_linux_binary(&old, STEAM_RUNTIME_GLIBC).expect("2.31 fits the sniper floor");
        assert_eq!(report.max_glibc, GlibcVersion::parse("2.31"));
        assert_eq!(report.max_glibc_from.as_deref(), Some("libc.so.6"));
        assert!(report.needed.is_empty(), "the fixture has no dynamic section");
        let new = dir.join("new");
        std::fs::write(
            &new,
            elf_with_needs(&[("libc.so.6", &["GLIBC_2.31", "GLIBC_2.34"])]),
        )
        .expect("write");
        let err = check_linux_binary(&new, STEAM_RUNTIME_GLIBC).expect_err("2.34 is above the floor");
        let text = format!("{err:#}");
        assert!(text.contains("needs GLIBC_2.34 (from libc.so.6)"), "{text}");
        assert!(text.contains("--container steam-sniper|debian12"), "{text}");
        // The Debian 12 floor admits it.
        check_linux_binary(&new, GlibcVersion { major: 2, minor: 36 }).expect("2.34 fits 2.36");
        // Not an ELF file: an error, not a panic.
        let text_file = dir.join("script.sh");
        std::fs::write(&text_file, "#!/bin/sh\n").expect("write");
        assert!(check_linux_binary(&text_file, STEAM_RUNTIME_GLIBC).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newest_glibc_version_is_found() {
        assert_eq!(max_glibc_version(OBJDUMP_T), GlibcVersion::parse("2.28"));
        assert_eq!(max_glibc_version("no symbols"), None);
        // 2.4x sorts above 2.31 numerically, not lexically.
        let newer = format!("{OBJDUMP_T}0 DF *UND* 0 (GLIBC_2.44) __isoc23_strtol\n");
        assert_eq!(
            max_glibc_version(&newer),
            Some(GlibcVersion { major: 2, minor: 44 })
        );
        assert!(GlibcVersion::parse("2.9").unwrap_or(STEAM_RUNTIME_GLIBC) < STEAM_RUNTIME_GLIBC);
        assert!(GlibcVersion::parse("2.44").unwrap_or(STEAM_RUNTIME_GLIBC) > STEAM_RUNTIME_GLIBC);
        assert_eq!(GlibcVersion::parse("host"), None);
    }

    #[test]
    fn needed_entries_are_parsed() {
        let headers = "\
Dynamic Section:
  NEEDED               libudev.so.1
  NEEDED               libasound.so.2
  NEEDED               libssl.so.3
  RUNPATH              $ORIGIN
";
        let needed = needed_libraries(headers);
        assert_eq!(needed, ["libudev.so.1", "libasound.so.2", "libssl.so.3"]);
        let outside: Vec<_> = needed
            .iter()
            .filter(|lib| !STEAM_RUNTIME_LIBS.contains(&lib.as_str()))
            .collect();
        assert_eq!(outside, ["libssl.so.3"]);
    }

    #[test]
    fn build_machine_strings_are_found() {
        let needles = forbidden_strings(
            Path::new("/home/dev/src/game"),
            Some(Path::new("/home/dev")),
            Some(Path::new("/home/dev/.cargo")),
        );
        assert_eq!(needles.len(), 4);
        let clean = b"crates/regia_render/src/lib.rs\0/cargo/registry/src/wgpu\0/home\0";
        assert!(find_forbidden(clean, &needles).is_empty());
        let leaky = b"xx/home/dev/.cargo/registry/src/naga-27/src/lib.rs\0";
        let found = find_forbidden(leaky, &needles);
        assert_eq!(found.len(), 2, "cargo home and home: {found:?}");
        let watcher = b"\0REGIA_ASSET_WATCH\0";
        assert_eq!(find_forbidden(watcher, &needles).len(), 1);
        // A root-sized home is not a usable needle.
        assert_eq!(
            forbidden_strings(Path::new("/w"), Some(Path::new("/")), None).len(),
            2
        );
    }

    #[test]
    fn agent_server_in_the_binary_fails_unless_allowed() {
        let bin = b"\x7fELF....regia-agent-accept....";
        assert_eq!(find_feature_markers(bin, false).len(), 1);
        assert!(find_feature_markers(bin, true).is_empty());
        let editor = b"....crates/regia_editor/src/main.rs....";
        assert_eq!(find_feature_markers(editor, true).len(), 1);
        assert!(find_feature_markers(b"clean binary", false).is_empty());
    }

    /// A runtime crate's cooked type path that names the cook crate (the terrain's tiled heightfield) is not the
    /// importer (ADR-5382): it alone passes, and an importer marker next to it still fails.
    #[test]
    fn a_runtime_cooked_type_path_is_not_the_importer() {
        let path = regia_terrain::stream::TILED_HEIGHTFIELD_TYPE_PATH;
        assert!(path.starts_with("regia_asset_pipeline::"), "the case this guards");
        let runtime = format!("\0tiles do not cover{path}class (painting needs one)\0{path}\0");
        assert!(
            find_feature_markers(runtime.as_bytes(), true).is_empty(),
            "{:?}",
            find_feature_markers(runtime.as_bytes(), true)
        );
        for linked in [
            "regia_asset_pipeline::gltf_import::import",
            "crates/regia_asset_pipeline/src/cook.rs",
        ] {
            let bin = format!("{runtime}{linked}\0");
            assert_eq!(find_feature_markers(bin.as_bytes(), true).len(), 1, "{linked}");
        }
        // Only the exact string is masked: the bytes around it are scanned as before.
        let around = format!("crates/regia_asset_pipeline/src{path}regia_editor::");
        let masked = mask_exact(around.as_bytes(), RUNTIME_TYPE_PATHS);
        assert!(masked.starts_with(b"crates/regia_asset_pipeline/src"));
        assert!(masked.ends_with(b"regia_editor::"));
        assert_eq!(find_feature_markers(around.as_bytes(), true).len(), 2);
    }

    #[test]
    fn explicit_cargo_features_are_rejected() {
        let ok = "[dependencies]\nregia = { path = \"x\", default-features = false, features = [\"ui\"] }\n";
        assert!(check_cargo_features(ok, false).is_ok());
        let agent = "[dependencies]\nregia = { path = \"x\", features = [\"agent\"] }\n";
        assert!(check_cargo_features(agent, false).is_err());
        assert!(check_cargo_features(agent, true).is_ok());
        let tests = "[dependencies]\nregia = { path = \"x\", features = [\"functional-tests\"] }\n";
        assert!(check_cargo_features(tests, false).is_err());
        assert!(check_cargo_features(tests, true).is_ok());
        let watch = "[dependencies]\nregia = { path = \"x\", features = [\"dev-assets\"] }\n";
        assert!(check_cargo_features(watch, true).is_err());
        assert!(check_cargo_features("[package]\nname = \"g\"\n", false).is_ok());
        // A project from before the rename that kept its dependency name is checked too.
        let legacy = regia_core::brand::LEGACY_MAIN_CRATE;
        let main = regia_core::brand::MAIN_CRATE;
        let renamed = format!(
            "[dependencies]\n{legacy} = {{ package = \"{main}\", path = \"x\", features = [\"dev-assets\"] }}\n"
        );
        assert!(check_cargo_features(&renamed, true).is_err());
        let renamed_ok = format!(
            "[dependencies]\n{legacy} = {{ package = \"{main}\", path = \"x\", features = [\"ui\"] }}\n"
        );
        assert!(check_cargo_features(&renamed_ok, false).is_ok());
    }

    #[test]
    fn plain_shipping_only_warns_about_the_agent_marker() -> Result<()> {
        let path = std::env::temp_dir().join(format!("regia_shipcheck_bin_{}", std::process::id()));
        std::fs::write(&path, format!("xx{AGENT_SERVER_MARKER}xx"))?;
        check_binary_features(&path, false, false)?;
        assert!(check_binary_features(&path, false, true).is_err());
        check_binary_features(&path, true, true)?;
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn editor_content_detection() {
        assert!(is_editor_content("assets/editor/layout.ron"));
        assert!(is_editor_content(".regia/cooked/x.fckd"));
        assert!(is_editor_content("assets/levels/main.autosave.ron"));
        assert!(!is_editor_content("assets/levels/main.level.ron"));
        assert!(is_editor_content(
            "assets/levels/forest.level.d/entities/8f/x.entity.ron"
        ));
        assert!(is_editor_content("assets/levels/forest.level.d"));
        assert!(
            !is_editor_content("assets/ui/editor/panel.png"),
            "only root-level editor dirs"
        );
        assert!(
            !is_editor_content("assets/editor"),
            "a file named editor is not a directory"
        );
    }

    #[test]
    fn stray_pak_and_editor_content_fail_the_tree_check() -> Result<()> {
        let root = std::env::temp_dir().join(format!("regia_shipcheck_tree_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let content = root.join("content");
        std::fs::create_dir_all(&content)?;
        std::fs::write(content.join("base.fpak"), b"x")?;
        let expected = vec!["base.fpak".to_string()];
        check_package_tree(&root, &content, &expected)?;

        std::fs::write(content.join("old_chunk.fpak"), b"x")?;
        let err = check_package_tree(&root, &content, &expected)
            .expect_err("must fail")
            .to_string();
        assert!(
            err.contains("stray pak") && err.contains("content/old_chunk.fpak"),
            "{err}"
        );
        std::fs::remove_file(content.join("old_chunk.fpak"))?;

        std::fs::write(root.join("extra.fpak"), b"x")?;
        assert!(
            check_package_tree(&root, &content, &expected).is_err(),
            "pak outside content/"
        );
        std::fs::remove_file(root.join("extra.fpak"))?;

        std::fs::create_dir_all(root.join("assets/editor"))?;
        std::fs::write(root.join("assets/editor/layout.ron"), b"x")?;
        let err = check_package_tree(&root, &content, &expected)
            .expect_err("must fail")
            .to_string();
        assert!(err.contains("editor content"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn importer_markers_in_the_binary_are_found() {
        for marker in IMPORTER_MARKERS {
            let bin = format!("\x7fELF..{marker}..");
            let found = find_feature_markers(bin.as_bytes(), true);
            assert_eq!(found.len(), 1, "{marker}: {found:?}");
            assert!(found[0].contains("importer"), "{found:?}");
        }
        // The runtime asset crate and the KTX2 reader are not the importer.
        let runtime = b"..regia_asset::cooked_pack..crates/regia_asset/src/pak.rs..ktx2-0.4.0/src..";
        assert!(find_feature_markers(runtime, true).is_empty());
    }

    #[test]
    fn importer_dependencies_are_found_only_at_runtime() -> Result<()> {
        let clean = "[dependencies]\nregia = { path = \"x\" }\n\
                     [build-dependencies]\nregia_asset_pipeline = { path = \"y\" }\n\
                     [dev-dependencies]\ngltf = \"1\"\n";
        assert!(runtime_importer_dependencies(clean)?.is_empty());
        check_cargo_runtime_importers(clean, true)?;

        let runtime = "[dependencies]\nregia = { path = \"x\" }\ngltf = \"1.4\"\n\
                       [target.'cfg(windows)'.dependencies]\nimporter = { package = \"regia_asset_pipeline\", path = \"y\" }\n";
        assert_eq!(
            runtime_importer_dependencies(runtime)?,
            ["gltf", "regia_asset_pipeline"]
        );
        let err = check_cargo_runtime_importers(runtime, true).expect_err("enforced");
        assert!(err.to_string().contains("gltf, regia_asset_pipeline"), "{err}");
        // The plain shipping profile only warns.
        check_cargo_runtime_importers(runtime, false)?;
        assert!(runtime_importer_dependencies("not toml [").is_err());
        Ok(())
    }

    #[test]
    fn cooked_pack_report_of_no_paks_is_clean() -> Result<()> {
        let report = check_cooked_paks(&[], "linux")?;
        assert_eq!((report.paks.len(), report.errors, report.warnings), (0, 0, 0));
        assert!(report.to_text().is_empty());
        Ok(())
    }

    #[test]
    fn cooked_pack_report_refuses_a_broken_pak() -> Result<()> {
        let dir = regia_test_utils::unique_temp_dir("shipcheck_broken_pak");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("base.fpak"), b"not a pak")?;
        let paks = paks_in(&dir)?;
        assert_eq!(paks.len(), 1);
        let err = cooked_pack_report(&paks, "linux").expect_err("a broken pak does not open");
        assert!(format!("{err:#}").contains("base.fpak"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// A pak holding one 64 × 64 base-color texture cooked for `cook_platform`, in a fresh temp dir.
    fn texture_pak(tag: &str, cook_platform: &str) -> Result<(PathBuf, PathBuf)> {
        use regia_asset::{
            AssetId, AssetManifest, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, ManifestEntry,
            TextureCookUsage, cooked_file_name, encode_cooked_file, pack_importer_version,
        };
        let dir = regia_test_utils::unique_temp_dir(tag);
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache)?;
        let rgba: Vec<u8> = (0..64 * 64 * 4).map(|i| (i * 7 % 251) as u8).collect();
        let texture = regia_asset_pipeline::cook_texture_rgba8(
            64,
            64,
            &rgba,
            TextureCookUsage::ColorSrgb,
            cook_platform,
            regia_asset_pipeline::TextureCookQuality::Low,
        )?;
        let payload = texture.to_postcard()?;
        let id = AssetId::new_random();
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::texture().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        std::fs::write(
            cache.join(cooked_file_name(id)),
            encode_cooked_file(&header, &payload)?,
        )?;
        let manifest = AssetManifest {
            entries: vec![ManifestEntry {
                path: "assets/rock_albedo.png".into(),
                id,
                type_path: "regia_asset::CookedTexture".into(),
            }],
        };
        manifest.write_to_cache(&cache)?;
        let pak = dir.join("base.fpak");
        regia_asset_pipeline::write_pak(
            &pak,
            &cache,
            &[(id, AssetTypeTag::texture().0)],
            &manifest,
            &regia_asset_pipeline::PakWriteOptions::default(),
        )?;
        Ok((dir, pak))
    }

    #[test]
    fn a_desktop_texture_fails_an_android_package_and_passes_a_desktop_one() -> Result<()> {
        let (dir, pak) = texture_pak("shipcheck_bc_on_android", "linux")?;
        let paks = [pak];
        let desktop = check_cooked_paks(&paks, "linux")?;
        assert_eq!((desktop.errors, desktop.warnings), (0, 0));
        let summary = &desktop.paks[0].summary;
        assert!(summary.textures_inspected);
        assert_eq!(summary.texture_families.get("bc"), Some(&1), "{summary:?}");

        let err = check_cooked_paks(&paks, "android").expect_err("BC is decoded on the CPU on Android");
        let text = format!("{err:#}");
        assert!(text.contains("texture_family_mismatch"), "{text}");
        assert!(
            text.contains("assets/rock_albedo.png"),
            "the example path is named: {text}"
        );
        let report = cooked_pack_report(&paks, "android")?;
        assert_eq!(report.errors, 1);
        assert_eq!(report.paks[0].findings[0].severity, Severity::Error);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// A one-entry lightmap whose entry has `seams`.
    fn seam_test_map(
        seams: Option<regia_asset::lightmap::LightmapSeams>,
    ) -> Result<regia_asset::lightmap::Lightmap> {
        use regia_asset::lightmap::{
            Coverage, Lightmap, LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap,
        };
        let layout = LightmapLayout {
            width: 4,
            height: 4,
            density: 1.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "wall".into(),
                source_vertices: 3,
                rect: [0, 0, 4, 4],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 1.0,
                texels_per_metre: 1.0,
                key: None,
                seams,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2],
                uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                indices: vec![0, 1, 2],
            }],
        };
        Ok(Lightmap::from_irradiance(
            layout,
            &[[1.0; 3]; 16],
            vec![Coverage::BAKED; 16],
            LightmapBakeInfo::default(),
        )?)
    }

    /// A staged pak holding one cooked lightmap whose entry has `seams`.
    fn lightmap_pak(
        tag: &str,
        seams: Option<regia_asset::lightmap::LightmapSeams>,
    ) -> Result<(PathBuf, PathBuf)> {
        use regia_asset::{
            AssetId, AssetManifest, COOKED_FORMAT_VERSION, CookedHeader, ManifestEntry, cooked_file_name,
            encode_cooked_file, pack_importer_version,
        };
        let dir = regia_test_utils::unique_temp_dir(tag);
        let payload = seam_test_map(seams)?.to_bytes()?;
        let id = AssetId::new_random();
        let type_path = "regia_asset::lightmap::Lightmap";
        let tag = regia_core::id::TypePathHash::of(type_path);
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: tag,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache)?;
        std::fs::write(
            cache.join(cooked_file_name(id)),
            encode_cooked_file(&header, &payload)?,
        )?;
        let manifest = AssetManifest {
            entries: vec![ManifestEntry {
                path: "assets/maps/yard.flmap".into(),
                id,
                type_path: type_path.into(),
            }],
        };
        manifest.write_to_cache(&cache)?;
        let pak = dir.join("base.fpak");
        regia_asset_pipeline::write_pak(
            &pak,
            &cache,
            &[(id, tag)],
            &manifest,
            &regia_asset_pipeline::PakWriteOptions::default(),
        )?;
        Ok((dir, pak))
    }

    /// The budget fails on the entry's stored p95, names it, and lets an older file or a project
    /// with no lightmap through.
    #[test]
    fn the_seam_budget_fails_a_lightmap_past_it() -> Result<()> {
        use regia_asset::lightmap::LightmapSeams;
        let seams = LightmapSeams {
            edges: 40,
            mean: 0.1,
            p95: 0.3,
            max: 0.9,
            worst_chart: 7,
        };
        let (dir, pak) = lightmap_pak("ship_check_seams", Some(seams))?;
        let paks = [pak];
        let err = check_lightmap_seams(&paks, 0.25).expect_err("past the budget");
        let text = format!("{err:#}");
        assert!(text.starts_with("lightmap_seam_budget"), "{text}");
        assert!(text.contains("assets/maps/yard.flmap wall"), "{text}");
        assert!(text.contains("worst chart 7"), "{text}");
        let ok = check_lightmap_seams(&paks, 0.5)?;
        assert_eq!((ok.lightmaps, ok.measured, ok.unmeasured), (1, 1, 0));
        assert!(ok.over.is_empty());
        std::fs::remove_dir_all(&dir)?;

        let (dir, pak) = lightmap_pak("ship_check_seams_old", None)?;
        let old = check_lightmap_seams(&[pak], 0.0)?;
        assert_eq!(
            (old.measured, old.unmeasured),
            (0, 1),
            "not measured is not a failure"
        );
        std::fs::remove_dir_all(&dir)?;

        let none = check_lightmap_seams(&[], 0.0)?;
        assert_eq!(none.lightmaps, 0, "opt-in: no lightmap, nothing to check");
        Ok(())
    }

    /// The figures survive what a shipped lightmap really goes through: the cook re-encodes the
    /// file and the pak writer packs it. An older file in the same project stays unmeasured. If the
    /// cook's manifest path or payload changed shape, this budget would quietly see no lightmap.
    #[test]
    fn a_really_cooked_lightmap_reaches_the_seam_budget() -> Result<()> {
        use regia_asset::lightmap::LightmapSeams;
        let root = regia_test_utils::unique_temp_dir("ship_check_seams_cooked");
        let maps = root.join("assets/maps");
        std::fs::create_dir_all(&maps)?;
        let seams = LightmapSeams {
            edges: 40,
            mean: 0.1,
            p95: 0.3,
            max: 0.9,
            worst_chart: 7,
        };
        seam_test_map(Some(seams))?.write(&maps.join("yard.flmap"))?;
        // Two entries, from v0.2.10's encoder.
        std::fs::write(
            maps.join("old.flmap"),
            include_bytes!("../../../regia_asset/tests/fixtures/lightmap-v2.flmap"),
        )?;
        let platform = regia_asset_pipeline::host_platform().to_string();
        let cook = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
            project_root: root.clone(),
            platform: platform.clone(),
            incremental: false,
        })?;
        assert!(cook.errors.is_empty(), "{:?}", cook.errors);
        let cooked = regia_asset::cooked_cache_dir(&root, &platform);
        let manifest = regia_asset::AssetManifest::read_from_cache(&cooked)?.context("a manifest")?;
        let entries: Vec<_> = manifest
            .entries
            .iter()
            .map(|e| (e.id, regia_core::id::TypePathHash::of(&e.type_path)))
            .collect();
        let pak = root.join("base.fpak");
        regia_asset_pipeline::write_pak(
            &pak,
            &cooked,
            &entries,
            &manifest,
            &regia_asset_pipeline::PakWriteOptions::default(),
        )?;
        let paks = [pak];
        let report = lightmap_seam_report(&paks, 0.25)?;
        assert_eq!((report.lightmaps, report.measured, report.unmeasured), (2, 1, 2));
        assert_eq!(report.over.len(), 1);
        assert_eq!(report.over[0].lightmap, "assets/maps/yard.flmap");
        assert_eq!(report.over[0].seams, seams);
        let err = check_lightmap_seams(&paks, 0.25).expect_err("past the budget");
        assert!(format!("{err:#}").starts_with("lightmap_seam_budget"), "{err:#}");
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn substring_search_handles_edges() {
        assert!(contains(b"abc", b""));
        assert!(contains(b"abc", b"c"));
        assert!(contains(b"aab", b"ab"));
        assert!(!contains(b"ab", b"abc"));
        assert!(!contains(b"", b"a"));
    }

    /// The counting fixture of `ship_check`: `n` surface graphs.
    fn graphs(n: u64) -> Vec<regia_render::permutation_policy::GraphPermutationInput> {
        (0..n)
            .map(|key| regia_render::permutation_policy::GraphPermutationInput {
                key,
                name: format!("assets/g{key}.matgraph.ron"),
                vertex: false,
                uv1: false,
                code_bytes: 512,
                double_sided: false,
            })
            .collect()
    }

    #[test]
    fn a_profile_over_its_permutation_budget_fails_shipping_and_warns_dev() {
        let mut budgets = std::collections::BTreeMap::new();
        budgets.insert(
            "MobileLow".to_string(),
            regia_render::PermutationBudget {
                max_pipelines: 10,
                max_compile_ms: u64::MAX,
            },
        );
        // Two graphs: 12 pipelines on Mobile Low (no GPU-driven variants), budget 10.
        let manifest = regia_render::PermutationManifest::enumerate("android", &graphs(2), &budgets);
        let err = check_permutation_manifest(&manifest, true).expect_err("over budget");
        let text = format!("{err}");
        assert!(
            text.contains("MobileLow: 12 material-graph pipelines, budget 10"),
            "{text}"
        );
        assert!(!text.contains("MobileHigh"), "{text}");
        assert!(check_permutation_manifest(&manifest, false).is_ok());
        let fits = regia_render::PermutationManifest::enumerate("android", &graphs(1), &budgets);
        assert!(check_permutation_manifest(&fits, true).is_ok());
    }

    #[test]
    fn a_cache_without_a_manifest_is_enumerated_from_the_asset_database() -> Result<()> {
        let dir = regia_test_utils::unique_temp_dir("ship_check_permutations");
        let cache = regia_asset::cooked_cache_dir(&dir, "linux");
        std::fs::create_dir_all(&cache)?;
        let manifest = check_permutation_budget(&dir, "linux", true)?;
        assert_eq!(manifest.graphs, 0);
        // Saved, so the package stages it.
        let saved =
            regia_asset_pipeline::permutation_cook::read_manifest(&cache).map_err(anyhow::Error::msg)?;
        assert_eq!(saved.as_ref(), Some(&manifest));
        assert_eq!(
            manifest
                .profiles
                .iter()
                .map(|p| p.profile.as_str())
                .collect::<Vec<_>>(),
            ["DesktopLow", "DesktopHigh", "DesktopUltra", "Deck"]
        );
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn an_overrun_on_an_unshipped_platform_warns_and_on_a_shipped_one_fails() {
        let mut budgets = std::collections::BTreeMap::new();
        budgets.insert(
            "MobileLow".to_string(),
            regia_render::PermutationBudget {
                max_pipelines: 10,
                max_compile_ms: u64::MAX,
            },
        );
        let android = regia_render::PermutationManifest::enumerate("android", &graphs(2), &budgets);
        let linux = regia_render::PermutationManifest::enumerate("linux", &graphs(2), &budgets);
        let manifests = [android.clone(), linux];
        // The project ships linux only: android's overrun is off target, linux fits.
        let findings = budget_findings(&manifests, &[], &["linux".to_string()]);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].code, PERMUTATION_BUDGET_OFF_TARGET);
        assert_eq!(findings[0].platform, "android");
        assert!(
            findings[0]
                .message()
                .starts_with("permutation_budget_off_target: android graph pipelines")
        );
        // It ships android too: the same overrun is now the failing code.
        let findings = budget_findings(&manifests, &[], &["linux".to_string(), "android".to_string()]);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].code, PERMUTATION_BUDGET_EXCEEDED);
        // A shipped-shader-bytes overrun sorts the same way.
        let cost = regia_render::ShaderCostReport {
            format_version: regia_render::SHADER_COST_REPORT_VERSION,
            platform: "android".to_string(),
            profiles: vec![regia_render::shader_cost_row(
                "android",
                regia_render::RenderProfilePreset::MobileLow,
                regia_render::ShaderBytesBudget { max_wgsl_bytes: 1 },
                0,
            )],
        };
        let findings = budget_findings(&[], std::slice::from_ref(&cost), &["linux".to_string()]);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].code, PERMUTATION_BUDGET_OFF_TARGET);
        assert_eq!(findings[0].kind, "shipped shader bytes");
        let findings = budget_findings(&[], &[cost], &["android".to_string()]);
        assert_eq!(findings[0].code, PERMUTATION_BUDGET_EXCEEDED);
    }
}
