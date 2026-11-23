// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask demo-package`: the downloadable free-camera demo, built for Linux and Windows and laid
//! out for upload (ADR-2827). Nothing is uploaded and no credential is read.
//!
//! ```text
//! <out>/                                 default target/demo-package
//!   payload/<target>/                    what is added to each bundle: content/scene/**, run-demo.*,
//!                                        CREDITS.txt, licenses/EULA.md
//!   readme/<target>/README.txt           the player README each bundle ships as README.txt (ADR-5002)
//!   bundles/<target>/                    `cargo xtask dist` output (bundle folder + archive + dist-manifest.json)
//!   depot/content/{windows,linux}/       the bundle folders, the Steam content roots
//!   depot/scripts/                       app_build_<appid>.vdf, depot_build_<depotid>.vdf per platform
//!   archive/                             regia-demo-<version>-{windows.zip,linux.tar} + CHECKSUMS.b3
//!   DECK-CHECK.md                        the Steam Deck check steps
//!   WINDOWS-CHECK.md                     the Windows minimum-spec check steps (ADR-5578)
//!   demo-package.json                    what was built (targets, files, checks run)
//! ```
//!
//! The build fails, before anything is compiled, when:
//!
//! - any file name, manifest row or the scene path itself names Direct Order 27 content;
//! - a scene file has no row in `asset_manifest.json` or its row has no licence;
//! - `[profile.shipping]` of the workspace `Cargo.toml` is not `panic = "abort"`;
//! - `--from-windows` / `--from-linux` is given without `--git-windows` / `--git-linux` (the build id of
//!   binaries made elsewhere is not the repository's HEAD; ADR-5002).
//!
//! Each bundle's `licenses/` holds the engine licence, the demo's own third-party notices (what the demo binary links,
//! `docs/demo/THIRD_PARTY_NOTICES-demo.md` through `dist --notices`) and the EULA (`docs/demo/EULA.md` once the owner's
//! text lands, the draft until then; `cargo xtask demo-notices --verify <depot>` refuses a draft) (ADR-5419).
//!
//! The depot is what players install, so it holds no developer files: the README is the player README
//! (build id, start, controls, privacy, where files go), and the Windows bundle has no `run-<bin>.cmd`
//! (`dist --no-launcher`; `run-demo.cmd` and Steam's launch option start the demo).
//!
//! The bundles come from `xtask dist` (`--profile shipping`, the Linux build against the glibc 2.31 of the
//! Steam Linux Runtime, the PE and glibc checks, `dist-manifest.json`), with the scene and launchers added
//! through `dist --overlay`. The plain archives are the bundles' own zip and tar; `CHECKSUMS.b3` is written by
//! `regia_cli::distribution::release` and read back before the command returns. The VDF shapes are the
//! documented steamcmd `appbuild` and `DepotBuildConfig` documents (the same ones `regia package --steam`
//! writes); upload is the owner's step: `steamcmd +login <account> +run_app_build <abs path>/depot/scripts/app_build_<appid>.vdf`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use regia_cli::distribution::{CHECKSUMS_FILE, checksum_files, checksums_text, verify_checksums};
use serde_json::{Value, json};

use crate::dist::{self, Target};

/// File name of the scene's asset manifest (the one the demo's credits page reads).
pub const ASSET_MANIFEST: &str = "asset_manifest.json";

/// Where the scene lands inside a bundle.
pub const SCENE_DEST: &str = "content/scene";

/// The EULA the owner approved, relative to the workspace root; until it exists the package ships the draft
/// ([`crate::demo_legal::EULA_PATH`]) and `demo-notices --verify` reports it (ADR-5419).
pub const EULA_FINAL_PATH: &str = "docs/demo/EULA.md";

/// Where each bundle carries the EULA.
pub const EULA_DEST: &str = "licenses/EULA.md";

/// Where each bundle carries the demo's third-party notices (`dist`'s licence file, replaced by `--notices`).
pub const NOTICES_DEST: &str = "licenses/THIRD_PARTY_NOTICES.md";

/// The first line of the demo notices `demo-notices` renders: a bundle whose notices start otherwise ships the
/// engine-wide list, not the demo's.
pub const DEMO_NOTICES_TITLE: &str = "# Third-party notices: Regia Engine showcase demo";

/// The EULA and the demo notices the package ships: the approved EULA when it exists, else the draft.
///
/// # Errors
/// `demo_package_legal_missing` when either file is missing or empty.
pub fn legal_sources(root: &Path) -> Result<(PathBuf, PathBuf)> {
    let eula = [EULA_FINAL_PATH, crate::demo_legal::EULA_PATH]
        .iter()
        .map(|p| root.join(p))
        .find(|p| p.is_file())
        .with_context(|| {
            format!(
                "demo_package_legal_missing: neither {EULA_FINAL_PATH} nor {} exists",
                crate::demo_legal::EULA_PATH
            )
        })?;
    let notices = root.join(crate::demo_legal::NOTICES_PATH);
    for path in [&eula, &notices] {
        let text = fs::read_to_string(path)
            .with_context(|| format!("demo_package_legal_missing: cannot read {}", path.display()))?;
        if text.trim().is_empty() {
            bail!("demo_package_legal_missing: {} is empty", path.display());
        }
    }
    Ok((eula, notices))
}

/// The licence problems of one bundle or depot folder: the EULA present and not empty, the notices the demo's own.
fn legal_problems(name: &str, dir: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    match fs::read_to_string(dir.join(EULA_DEST)) {
        Ok(text) if !text.trim().is_empty() => {}
        _ => problems.push(format!(
            "demo_package_legal: the {name} folder {} has no {EULA_DEST}",
            dir.display()
        )),
    }
    match fs::read_to_string(dir.join(NOTICES_DEST)) {
        Ok(text) if text.lines().next() == Some(DEMO_NOTICES_TITLE) => {}
        _ => problems.push(format!(
            "demo_package_legal: the {name} folder {} does not ship the demo notices as {NOTICES_DEST}",
            dir.display()
        )),
    }
    problems
}

/// Licence values that are no licence.
const EMPTY_LICENCES: &[&str] = &["", "unknown", "none", "tbd", "todo", "?", "n/a", "na"];

/// A platform of the demo package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum DemoTarget {
    /// Windows 10/11 x64.
    Windows,
    /// Linux x86-64 against glibc 2.31 (runs on the Steam Deck and in the Steam Linux Runtime).
    Linux,
}

impl DemoTarget {
    /// `windows` or `linux`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
        }
    }

    /// The `xtask dist` target that builds it.
    #[must_use]
    pub fn dist_target(self) -> Target {
        match self {
            Self::Windows => Target::Windows,
            Self::Linux => Target::Deck,
        }
    }
}

/// Command-line options of `cargo xtask demo-package`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// The cooked scene directory to ship (with its `asset_manifest.json`).
    #[arg(long, default_value = "samples/showcase/example_scene")]
    pub scene: PathBuf,
    /// Output directory (default `target/demo-package`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Platforms to build, comma separated.
    #[arg(long, value_enum, value_delimiter = ',', default_values_t = [DemoTarget::Windows, DemoTarget::Linux])]
    pub targets: Vec<DemoTarget>,
    /// The binary to ship, `<package>` or `<package>:<bin>`.
    #[arg(long, default_value = "showcase")]
    pub bin: String,
    /// The Steam app id for the depot scripts. Without it the scripts carry `REPLACE_WITH_APP_ID` and the
    /// owner fills it in; Valve numbers the depots `app id + 1` (Windows) and `+ 2` (Linux).
    #[arg(long)]
    pub app_id: Option<u32>,
    /// Windows binaries built elsewhere (skips the Windows build).
    #[arg(long, value_name = "DIR")]
    pub from_windows: Option<PathBuf>,
    /// Linux binaries built elsewhere (skips the Linux build).
    #[arg(long, value_name = "DIR")]
    pub from_linux: Option<PathBuf>,
    /// The commit the `--from-windows` binaries were built from (required with it; recorded in the
    /// bundle manifest and the player README).
    #[arg(long, value_name = "REV", requires = "from_windows")]
    pub git_windows: Option<String>,
    /// The commit the `--from-linux` binaries were built from (required with it).
    #[arg(long, value_name = "REV", requires = "from_linux")]
    pub git_linux: Option<String>,
    /// Only run the content and profile checks (no build, nothing written).
    #[arg(long)]
    pub check: bool,
    /// Check a finished package directory: the checksum list, every bundle's `dist-manifest.json` and the
    /// depot scripts' content roots; then exit.
    #[arg(long, value_name = "DIR")]
    pub verify: Option<PathBuf>,
    /// Opt in to Intel XeSS in the Windows bundle (ADR-7820): stage the unmodified `bin/libxess.dll`,
    /// `LICENSE.txt` and `third-party-programs.txt` of this absolute XeSS SDK root, add the XeSS section
    /// to that bundle's notices, and build with `regia/xess` (export `XESS_SDK` with the same path for
    /// the binding; `--from-windows` binaries must have been built that way). Off by default: shipping
    /// the vendor runtime is the owner's decision.
    #[arg(long, value_name = "DIR")]
    pub xess_sdk: Option<PathBuf>,
}

/// Where a bundle carries Intel's runtime and terms with `--xess-sdk` (the runtime beside the binary).
pub const XESS_FILES: [&str; 3] = [
    "libxess.dll",
    "INTEL-XESS-LICENSE.txt",
    "INTEL-XESS-THIRD-PARTY-PROGRAMS.txt",
];

/// The SDK files `--xess-sdk` stages, `(source, name in the bundle)` in [`XESS_FILES`] order: each must be a
/// nonempty file of the absolute SDK root `sdk`.
///
/// # Errors
/// `xess_package_invalid` for a relative root or a missing or empty file.
pub fn xess_sources(sdk: &Path) -> Result<Vec<(PathBuf, &'static str)>> {
    if !sdk.is_absolute() {
        bail!("xess_package_invalid: XESS_SDK must be an absolute path");
    }
    let sources = [
        sdk.join("bin").join("libxess.dll"),
        sdk.join("LICENSE.txt"),
        sdk.join("third-party-programs.txt"),
    ];
    let mut out = Vec::new();
    for (source, name) in sources.into_iter().zip(XESS_FILES) {
        let metadata = fs::symlink_metadata(&source)
            .with_context(|| format!("xess_package_invalid: reading {}", source.display()))?;
        if !metadata.is_file() || metadata.len() == 0 {
            bail!(
                "xess_package_invalid: {} must be a nonempty regular file",
                source.display()
            );
        }
        out.push((source, name));
    }
    Ok(out)
}

/// The problems of one `--xess-sdk` bundle or depot folder (`xess`) or of a plain one: with XeSS, every
/// [`XESS_FILES`] entry is present and nonempty and the notices carry the XeSS section; without it, no
/// XeSS file is there at all.
pub fn xess_problems(name: &str, dir: &Path, xess: bool) -> Vec<String> {
    let mut problems = Vec::new();
    for file in XESS_FILES {
        let present = fs::metadata(dir.join(file)).is_ok_and(|m| m.is_file() && m.len() > 0);
        if xess && !present {
            problems.push(format!(
                "demo_package_xess: the {name} folder {} has no {file}",
                dir.display()
            ));
        } else if !xess && fs::symlink_metadata(dir.join(file)).is_ok() {
            problems.push(format!(
                "demo_package_xess: the {name} folder {} ships {file} without --xess-sdk",
                dir.display()
            ));
        }
    }
    if xess {
        let notices = fs::read_to_string(dir.join(NOTICES_DEST)).unwrap_or_default();
        if !notices.contains(crate::notices::XESS_NOTICE.trim()) {
            problems.push(format!(
                "demo_package_xess: the {name} notices have no Intel XeSS section"
            ));
        }
    }
    problems
}

/// One row of `asset_manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetRow {
    /// Path relative to the scene directory, `/` separators.
    pub path: String,
    /// The licence id (may be empty: that is a finding).
    pub licence: String,
    /// All row text, joined, for the content scan.
    pub text: String,
}

/// The first Direct Order 27 marker in `text`, if any. Case, spaces, `-`, `_` and `.` do not matter, so
/// `DO27`, `do-27`, `Direct Order 27` and `direct_order` all match. Fails closed on a harmless
/// look-alike.
#[must_use]
pub fn do27_marker(text: &str) -> Option<&'static str> {
    let squashed: String = text
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    ["directorder", "do27"].into_iter().find(|m| squashed.contains(m))
}

/// What [`check_content`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentReport {
    /// The scene's files (not the manifest), relative, `/` separators, sorted.
    pub files: Vec<String>,
    /// Findings; the package fails when any exists.
    pub problems: Vec<String>,
}

/// Parses the rows of an asset manifest.
///
/// # Errors
/// Text that is not JSON with an `assets` array of objects that each have a string `path`.
pub fn parse_manifest(text: &str) -> Result<Vec<AssetRow>> {
    let doc: Value = serde_json::from_str(text).context("asset_manifest.json is not JSON")?;
    let assets = doc
        .get("assets")
        .and_then(Value::as_array)
        .context("asset_manifest.json has no `assets` array")?;
    let mut rows = Vec::new();
    for (i, row) in assets.iter().enumerate() {
        let path = row
            .get("path")
            .and_then(Value::as_str)
            .with_context(|| format!("asset row {i} has no string `path`"))?;
        let licence = row
            .get("licence")
            .or_else(|| row.get("license"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let text = row
            .as_object()
            .map(|o| o.values().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        rows.push(AssetRow {
            path: path.to_string(),
            licence,
            text,
        });
    }
    Ok(rows)
}

/// Checks a scene directory for shipping: no Direct Order 27 content anywhere in its path, file names or
/// manifest rows, and every file with a manifest row that names a licence.
///
/// # Errors
/// An unreadable directory.
pub fn check_content(scene: &Path) -> Result<ContentReport> {
    let _span = tracing::info_span!("xtask::demo_package::check_content").entered();
    let mut report = ContentReport::default();
    let canonical = scene
        .canonicalize()
        .with_context(|| format!("--scene {}", scene.display()))?;
    if let Some(marker) = do27_marker(&canonical.to_string_lossy()) {
        report.problems.push(format!(
            "do27_content_path: the scene path {} names Direct Order 27 content ({marker}); \
             ship a scene that is not DO27's",
            canonical.display()
        ));
    }
    collect_files(&canonical, &canonical, &mut report)?;
    report.files.sort();
    for file in &report.files {
        if let Some(marker) = do27_marker(file) {
            report.problems.push(format!(
                "do27_content_path: {file} names Direct Order 27 content ({marker})"
            ));
        }
    }
    let manifest_path = canonical.join(ASSET_MANIFEST);
    let rows = match fs::read_to_string(&manifest_path) {
        Err(_) => {
            report.problems.push(format!(
                "asset_manifest_missing: {ASSET_MANIFEST} is not in the scene directory; every shipped asset needs a licence row"
            ));
            return Ok(report);
        }
        Ok(text) => match parse_manifest(&text) {
            Ok(rows) => rows,
            Err(err) => {
                report.problems.push(format!("asset_manifest_invalid: {err:#}"));
                return Ok(report);
            }
        },
    };
    let mut seen = BTreeSet::new();
    for row in &rows {
        if !seen.insert(row.path.as_str()) {
            report
                .problems
                .push(format!("asset_row_duplicate: {} has two rows", row.path));
        }
        if let Some(marker) = do27_marker(&row.text) {
            report.problems.push(format!(
                "do27_content_path: the row for {} names Direct Order 27 content ({marker})",
                row.path
            ));
        }
        if EMPTY_LICENCES.contains(&row.licence.to_ascii_lowercase().as_str()) {
            report.problems.push(format!(
                "asset_no_licence: the row for {} has no licence",
                row.path
            ));
        }
        if !report.files.contains(&row.path) {
            report.problems.push(format!(
                "asset_row_orphan: the row for {} names a file the scene does not have",
                row.path
            ));
        }
    }
    for file in &report.files {
        if !seen.contains(file.as_str()) {
            report.problems.push(format!(
                "asset_no_licence: {file} has no row in {ASSET_MANIFEST} (so no licence)"
            ));
        }
    }
    Ok(report)
}

/// Adds the files below `dir` to `report.files` (relative to `base`, the manifest excluded). A link or
/// special file is a finding: a shipped scene is plain files.
fn collect_files(base: &Path, dir: &Path, report: &mut ContentReport) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        let kind = entry.file_type().with_context(|| path.display().to_string())?;
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if kind.is_dir() {
            collect_files(base, &path, report)?;
        } else if kind.is_file() {
            if rel != ASSET_MANIFEST {
                report.files.push(rel);
            }
        } else {
            report
                .problems
                .push(format!("scene_special_file: {rel} is a link or special file"));
        }
    }
    Ok(())
}

/// Fails unless the workspace's `shipping` profile aborts on panic (ADR-2827 § 1).
///
/// # Errors
/// An unreadable manifest, or a profile that is missing or does not set `panic = "abort"`.
pub fn check_profile(root: &Path) -> Result<()> {
    let path = root.join("Cargo.toml");
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    check_profile_text(&text)
}

/// [`check_profile`] over the manifest text.
///
/// # Errors
/// As [`check_profile`].
pub fn check_profile_text(text: &str) -> Result<()> {
    let table: toml::Table = toml::from_str(text).context("Cargo.toml is not TOML")?;
    let shipping = table
        .get("profile")
        .and_then(|p| p.get("shipping"))
        .and_then(toml::Value::as_table)
        .context("shipping_profile_missing: Cargo.toml has no [profile.shipping]")?;
    if shipping.get("panic").and_then(toml::Value::as_str) != Some("abort") {
        bail!("shipping_not_abort: [profile.shipping] must set panic = \"abort\"");
    }
    Ok(())
}

/// The launcher for Linux (`run-demo.sh`): runs from the bundle folder, opens the demo on the bundled scene.
#[must_use]
pub fn launcher_sh(bin: &str) -> String {
    format!(
        "#!/bin/sh\n# Regia Engine free-camera demo launcher (written by cargo xtask demo-package).\n\
         cd \"$(dirname \"$0\")\" || exit 1\n\
         exec \"./{bin}\" --demo content/scene \"$@\"\n"
    )
}

/// The launcher for Windows (`run-demo.cmd`, CRLF).
#[must_use]
pub fn launcher_cmd(bin: &str) -> String {
    let lines = [
        "@echo off".to_string(),
        "rem Regia Engine free-camera demo launcher (written by cargo xtask demo-package).".to_string(),
        "setlocal".to_string(),
        "cd /d \"%~dp0\"".to_string(),
        format!("\"%~dp0{bin}.exe\" --demo content\\scene %*"),
        "set CODE=%ERRORLEVEL%".to_string(),
        "if not \"%CODE%\"==\"0\" (".to_string(),
        format!("  echo {bin}.exe exited with code %CODE%. See README.txt for the log folder."),
        "  pause".to_string(),
        ")".to_string(),
        "exit /b %CODE%".to_string(),
    ];
    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    text
}

/// The `CREDITS.txt` of a package: one line per manifest row.
#[must_use]
pub fn credits_text(rows: &[Value]) -> String {
    let mut text = String::from("Assets in this demo\n===================\n\n");
    for row in rows {
        let get = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("");
        let title = if get("title").is_empty() {
            get("path")
        } else {
            get("title")
        };
        let licence = if get("licence").is_empty() {
            get("license")
        } else {
            get("licence")
        };
        text.push_str(&format!(
            "{title} ({}) by {}: {licence}",
            get("path"),
            get("author")
        ));
        if !get("source").is_empty() {
            text.push_str(&format!(" - {}", get("source")));
        }
        text.push('\n');
    }
    text.push_str("\nEngine and library notices: licenses/THIRD_PARTY_NOTICES.md\n");
    text.push_str(&format!(
        "Built with Regia Engine, under the Mozilla Public License 2.0 (licenses/LICENSE); its source code is \
         available at {} (MPL-2.0 section 3.2).\n",
        crate::dist::ENGINE_SOURCE_URL
    ));
    text
}

/// `text` broken at spaces into lines of at most `width` characters, each starting with `indent`
/// and ending with a newline (a longer word stays whole).
#[must_use]
pub fn wrap(text: &str, width: usize, indent: &str) -> String {
    let mut out = String::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && indent.len() + line.len() + 1 + word.len() > width {
            out.push_str(&format!("{indent}{line}\n"));
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push_str(&format!("{indent}{line}\n"));
    }
    out
}

/// The player README each bundle ships as `README.txt` (ADR-5002): the build id, how to start, the
/// controls, what the demo does and does not send, and where it writes. `bin` is the binary's name
/// (no `.exe`), `git` the commit the binary was built from.
#[must_use]
pub fn player_readme(target: DemoTarget, bin: &str, version: &str, git: &str) -> String {
    let name = crate::steam_demo_upload::demo_name();
    let (platform, start, needs) = match target {
        DemoTarget::Windows => (
            "Windows 10 or 11 (64-bit)",
            "  From Steam: press Play.\n  Without Steam: double-click run-demo.cmd in this folder.\n",
            "a GPU driver with Vulkan or Direct3D 12",
        ),
        DemoTarget::Linux => (
            "Linux (64-bit) and SteamOS",
            "  From Steam: press Play.\n  Without Steam: run ./run-demo.sh in this folder.\n",
            "a Vulkan driver and glibc 2.31 or newer",
        ),
    };
    let mut text = format!(
        "{name} {version} for {platform}\nBuild {git}\n\n\
         Starting\n{start}  It needs {needs}.\n  \
         The first start prepares the graphics for your PC behind the loading screen: on a low-end\n  \
         PC this can take a minute or more. Leave it running; later starts take a few seconds.\n\n\
         Controls (the same text as the demo's panel)\n{help}  On a gamepad: Select (View) opens the panel, North (Y / Triangle) takes a screenshot,\n  \
         Start runs the benchmark.\n  Screenshots and benchmark tables go to the folder the demo shows when it starts.\n\n\
         Privacy\n  The demo plays offline and needs no account. It sends nothing over the network: no\n  \
         telemetry and no crash reports, and it opens no network port.\n\n\
         Where it writes (these folders stay after an uninstall; delete them to remove everything)\n",
        help = wrap(showcase::demo::ui::HELP, 100, "  "),
    );
    for dir in target.dist_target().runtime_dirs() {
        let label = match dir.key {
            "user" => "Settings",
            "logs" => "Logs (a panic-* file here means the demo stopped on an error)",
            "cache" => "Cache (safe to delete)",
            _ => continue,
        };
        text.push_str(&format!("  {label}:\n    {}\n", dir.path.replace("<exe>", bin)));
    }
    text.push_str(
        "\nCredits: CREDITS.txt. Licences: the licenses folder.\n\
         dist-manifest.json lists every file of this folder with its size and BLAKE3 hash.\n",
    );
    text
}

/// The `app_build_<appid>.vdf` for the depot scripts (documented steamcmd `appbuild` shape). `depots`
/// is (depot id text, depot script file name). `setlive` is empty: nothing goes live until the owner
/// sets a branch.
#[must_use]
pub fn app_build_vdf(app_id: &str, desc: &str, depots: &[(String, String)]) -> String {
    let mut vdf = String::from("\"appbuild\"\n{\n");
    vdf.push_str(&format!("  \"appid\" \"{}\"\n", vdf_escape(app_id)));
    vdf.push_str(&format!("  \"desc\" \"{}\"\n", vdf_escape(desc)));
    vdf.push_str("  \"buildoutput\" \"../output/\"\n");
    vdf.push_str("  \"contentroot\" \"../content/\"\n");
    vdf.push_str("  \"setlive\" \"\"\n");
    vdf.push_str("  \"depots\"\n  {\n");
    for (id, file) in depots {
        vdf.push_str(&format!("    \"{}\" \"{}\"\n", vdf_escape(id), vdf_escape(file)));
    }
    vdf.push_str("  }\n}\n");
    vdf
}

/// The `depot_build_<id>.vdf` mapping `<contentroot>/<local_dir>/` into the depot root.
#[must_use]
pub fn depot_build_vdf(depot_id: &str, local_dir: &str) -> String {
    format!(
        "\"DepotBuildConfig\"\n{{\n  \"DepotID\" \"{}\"\n  \"FileMapping\"\n  {{\n    \
         \"LocalPath\" \"{}/*\"\n    \"DepotPath\" \".\"\n    \"recursive\" \"1\"\n  }}\n  \
         \"FileExclusion\" \"*.pdb\"\n}}\n",
        vdf_escape(depot_id),
        vdf_escape(local_dir)
    )
}

/// Escapes a VDF string value.
fn vdf_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The depot id of `target` for an app id: Windows `app + 1`, Linux `app + 2` (Valve's numbering of a
/// standard app's depots); the placeholder text when there is no app id.
#[must_use]
pub fn depot_id(app_id: Option<u32>, target: DemoTarget) -> String {
    match app_id {
        Some(app) => {
            let offset = match target {
                DemoTarget::Windows => 1,
                DemoTarget::Linux => 2,
            };
            app.saturating_add(offset).to_string()
        }
        None => format!("REPLACE_WITH_{}_DEPOT_ID", target.name().to_ascii_uppercase()),
    }
}

/// The Steam Deck check steps (`DECK-CHECK.md`).
#[must_use]
pub fn deck_check_md(version: &str) -> String {
    format!(
        "# Steam Deck check for the demo ({version})\n\n\
         Run these on the Deck itself, once per candidate build. Nothing here uploads anything.\n\n\
         1. Copy every file in `archive/`, including `regia-demo-{version}-linux.tar` and `{CHECKSUMS_FILE}`, to the Deck (USB or `scp`).\n\
         2. In Desktop Mode, open Konsole in an empty folder: `b3sum --check {CHECKSUMS_FILE}` (or compare the hash by hand;\n\
            `cargo xtask demo-package --verify <staged-dir>` checks the complete staging directory on the build host). Every line must say OK.\n\
            SteamOS ships no `b3sum`: there, compare `sha256sum regia-demo-{version}-linux.tar` with the same command on the\n\
            build host instead.\n\
         3. `tar xf regia-demo-{version}-linux.tar && cd regia-deck-{version}`; `./run-demo.sh --check` must print the scene\n\
            summary and exit 0 (no GPU needed).\n\
         4. `./run-demo.sh --fullscreen`: the window opens fullscreen, the first frame appears, the tier picker says in words\n\
            why it chose a tier (expect Deck or Low), the fps overlay shows a measured number.\n\
         5. Controller: left stick moves, right stick looks, the panel button opens the panel, North takes a screenshot (a\n\
            file appears in the printed folder), Start runs the benchmark and writes its table.\n\
         6. Quit from the panel. Exit code 0. `~/.local/share/RegiaEngine/showcase/logs/` has the session log; a file named\n\
            `panic-*` there means the check failed.\n\
         7. Game Mode: add the folder's `run-demo.sh` as a non-Steam game, launch it from Game Mode, and confirm the same\n\
            frame, the controller layout and a clean quit back to the library. (Steam Input layout is the owner's; the demo\n\
            reads the gamepad directly.)\n\
         8. Record under the build's name: frame time median and p99 from the benchmark table, the tier it chose, and any\n\
            step above that failed.\n"
    )
}

/// The Windows minimum-spec check steps (`WINDOWS-CHECK.md`, ADR-5578): the Deck's steps for the Windows zip, on
/// the weakest PC the demo supports, with the long cold first launch called out.
#[must_use]
pub fn windows_check_md(version: &str) -> String {
    format!(
        "# Windows check for the demo ({version})\n\n\
         Run these on the minimum-spec Windows PC (integrated graphics, 4 cores), once per candidate build. Nothing\n\
         here uploads anything.\n\n\
         1. Copy `archive/regia-demo-{version}-windows.zip` to the PC.\n\
         2. Windows has no `b3sum` or `sha256sum`: in `cmd`, `certutil -hashfile regia-demo-{version}-windows.zip SHA256`\n\
            (or PowerShell `Get-FileHash`) must print the same hash as `sha256sum regia-demo-{version}-windows.zip` on the\n\
            build host.\n\
         3. Unpack it with Explorer into a new folder whose path has a space and a non-ASCII letter (players do). Mark\n\
            the folder read-only if you can: the demo must not write next to itself.\n\
         4. In `cmd` in `regia-windows-{version}`: `run-demo.cmd --check` prints the scene summary; `echo %ERRORLEVEL%`\n\
            prints 0 (no GPU needed). A \"VCRUNTIME140.dll was not found\" dialog means the PC lacks the Visual C++\n\
            2015-2022 runtime, which Steam installs before the first launch (Redistributables).\n\
         5. Double-click `run-demo.cmd`. The first launch on a PC whose graphics driver has never seen this build\n\
            compiles every shader behind the loading screen: on an Intel N150 that took 79 s to the forest\n\
            (2026-11-10). It must not freeze (the loading screen keeps animating) and must not stutter once the\n\
            forest shows. Quit, then time the second launch: a few seconds (4.2 s on the N150).\n\
         6. The tier picker names the tier it chose and says why in words (expect Low on integrated graphics); the\n\
            fps overlay shows a measured number.\n\
         7. Keyboard and mouse: W A S D fly, holding the right mouse button looks, F1 opens the panel, F12 writes a\n\
            screenshot to the folder the demo names; with a pad, the left stick moves and the right stick looks.\n\
         8. Close the window: the process ends. `run-demo.cmd --exit-after-frames 30` starts, shows 30 frames of the\n\
            forest and ends by itself; `echo %ERRORLEVEL%` then prints 0. `%APPDATA%\\RegiaEngine\\showcase\\logs` has the\n\
            session log; a file named `panic-*` there means the check failed. Nothing was written into the unpacked\n\
            folder.\n\
         9. Record under the build's name: the first and second launch times to the forest, the tier it chose, and any\n\
            step above that failed.\n"
    )
}

/// The upload notes next to the depot scripts.
#[must_use]
pub fn upload_notes(app_id: Option<u32>) -> String {
    let id = app_id.map_or_else(|| "<appid>".to_string(), |a| a.to_string());
    let placeholder = if app_id.is_none() {
        "The scripts carry REPLACE_WITH_APP_ID and REPLACE_WITH_*_DEPOT_ID: rerun the package with the owner's\n\
         --app-id before upload. Editing scripts by hand invalidates complete-package verification.\n\n"
    } else {
        ""
    };
    format!(
        "Steam upload (done by the owner; the package never logs in)\n\n{placeholder}\
         1. Put steamcmd on PATH. The content roots are ../content/windows and ../content/linux relative to\n\
            these scripts.\n\
         2. `steamcmd +login <your build account> +run_app_build \"$PWD/app_build_{id}.vdf\" +quit`\n\
         3. `setlive` is empty: the build appears on the partner site and goes live when you set a branch there.\n\
         4. Upload the archives in ../../archive somewhere else if you also hand them out directly; CHECKSUMS.b3 lists their\n\
            BLAKE3 hashes (`b3sum --check CHECKSUMS.b3`).\n"
    )
}

/// Recursively copies `from` to `to` (plain files only; modes kept by `fs::copy`).
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &dest).with_context(|| format!("copying {}", entry.path().display()))?;
        } else {
            bail!("{} is a link or special file", entry.path().display());
        }
    }
    Ok(())
}

/// Every file below `dir`, relative with `/` separators, sorted.
fn list_files(dir: &Path) -> Result<Vec<String>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(base, &path, out)?;
            } else if kind.is_file() {
                let rel = path.strip_prefix(base).unwrap_or(&path);
                out.push(
                    rel.components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/"),
                );
            } else {
                bail!(
                    "demo_package_special_file: {} is a link or special file",
                    path.display()
                );
            }
        }
        Ok(())
    }
    if !fs::symlink_metadata(dir)?.is_dir() {
        bail!(
            "demo_package_special_file: {} is not a plain directory",
            dir.display()
        );
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// Writes `bytes` to `path` through a temporary sibling and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("partial");
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))
}

/// Stages the overlay for one target: the scene, the launcher and the credits.
fn stage_payload(
    args: &Args,
    target: DemoTarget,
    scene: &Path,
    payload: &Path,
    rows: &[Value],
    eula: &Path,
    xess: &[(PathBuf, &'static str)],
) -> Result<()> {
    if payload.exists() {
        fs::remove_dir_all(payload).with_context(|| format!("clearing {}", payload.display()))?;
    }
    copy_tree(scene, &payload.join(SCENE_DEST))?;
    let bin = args.bin.split_once(':').map_or(args.bin.as_str(), |(_, b)| b);
    match target {
        DemoTarget::Windows => write_atomic(&payload.join("run-demo.cmd"), launcher_cmd(bin).as_bytes())?,
        DemoTarget::Linux => write_atomic(&payload.join("run-demo.sh"), launcher_sh(bin).as_bytes())?,
    }
    let eula_text = fs::read(eula).with_context(|| format!("reading {}", eula.display()))?;
    write_atomic(&payload.join(EULA_DEST), &eula_text)?;
    // Intel's files, bytes unchanged (`--xess-sdk`, Windows only; ADR-7820).
    for (source, name) in xess {
        let bytes = fs::read(source)
            .with_context(|| format!("xess_package_invalid: reading {}", source.display()))?;
        write_atomic(&payload.join(name), &bytes)?;
    }
    write_atomic(&payload.join("CREDITS.txt"), credits_text(rows).as_bytes())
}

/// Runs `cargo xtask demo-package`.
///
/// # Errors
/// A failed content or profile check (listed all at once), a failed build, a bundle that fails its
/// checks, or IO.
pub fn run(args: &Args, root: &Path) -> Result<ExitCode> {
    if let Some(dir) = &args.verify {
        return verify_package(dir);
    }
    check_profile(root)?;
    let scene = if args.scene.is_absolute() {
        args.scene.clone()
    } else {
        root.join(&args.scene)
    };
    let report = check_content(&scene)?;
    if !report.problems.is_empty() {
        for p in &report.problems {
            eprintln!("error: {p}");
        }
        eprintln!(
            "demo-package: {} content problem(s); nothing was built",
            report.problems.len()
        );
        return Ok(ExitCode::FAILURE);
    }
    println!(
        "demo-package: {} scene file(s), every one with a licence row, none from Direct Order 27; shipping profile aborts on panic",
        report.files.len()
    );
    if args.check {
        return Ok(ExitCode::SUCCESS);
    }
    let mut targets = args.targets.clone();
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        bail!("--targets is empty");
    }
    // The EULA and the demo notices, before anything is cleared or built (ADR-5419).
    let (eula, notices) = legal_sources(root)?;
    // XeSS for the Windows bundle, checked before anything is cleared or built (ADR-7820).
    let xess = match &args.xess_sdk {
        Some(sdk) => {
            if !targets.contains(&DemoTarget::Windows) {
                bail!("xess_package_invalid: --xess-sdk stages a Windows runtime; add the windows target");
            }
            if args.from_windows.is_none() && std::env::var_os("XESS_SDK").as_deref() != Some(sdk.as_os_str())
            {
                bail!(
                    "xess_package_invalid: building the Windows demo with XeSS needs XESS_SDK={} for the binding",
                    sdk.display()
                );
            }
            xess_sources(sdk)?
        }
        None => Vec::new(),
    };
    // The build id of every target, before anything is cleared or built (ADR-5002).
    let mut gits = Vec::new();
    for target in &targets {
        let (from, given) = match target {
            DemoTarget::Windows => (&args.from_windows, &args.git_windows),
            DemoTarget::Linux => (&args.from_linux, &args.git_linux),
        };
        let name = target.name();
        if from.is_some() && given.is_none() {
            bail!(
                "demo_package_git_unknown: --from-{name} binaries need --git-{name} <rev>, the commit they were built \
                 from (the repository's HEAD is not their build)"
            );
        }
        let git = dist::bundle_git(given.as_deref(), root)?;
        if git == "unknown" {
            bail!(
                "demo_package_git_unknown: no build id for {name}: build from a git checkout, set REGIA_GIT_HASH, \
                 or pass --git-{name}"
            );
        }
        gits.push(git);
    }
    let version = dist::engine_version();
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| root.join("target").join("demo-package"));
    let out = if out.is_absolute() {
        out
    } else {
        std::env::current_dir()?.join(out)
    };
    if out.exists() {
        fs::remove_dir_all(&out).with_context(|| format!("clearing {}", out.display()))?;
    }
    let manifest_text =
        fs::read_to_string(scene.join(ASSET_MANIFEST)).context("reading the asset manifest")?;
    let manifest_doc: Value = serde_json::from_str(&manifest_text).context("asset manifest")?;
    let rows: Vec<Value> = manifest_doc["assets"].as_array().cloned().unwrap_or_default();

    let mut built = Vec::new();
    let mut depots = Vec::new();
    let mut archives = Vec::new();
    let bin = args.bin.split_once(':').map_or(args.bin.as_str(), |(_, b)| b);
    for (target, git) in targets.iter().zip(&gits) {
        let name = target.name();
        let payload = out.join("payload").join(name);
        let with_xess = *target == DemoTarget::Windows && !xess.is_empty();
        let staged_xess = if with_xess { xess.as_slice() } else { &[] };
        stage_payload(
            args,
            *target,
            &scene.canonicalize()?,
            &payload,
            &rows,
            &eula,
            staged_xess,
        )?;
        // The XeSS bundle's notices: the demo's own plus Intel's section.
        let bundle_notices = if with_xess {
            let text =
                fs::read_to_string(&notices).with_context(|| format!("reading {}", notices.display()))?;
            let path = out.join("notices").join(name).join(NOTICES_DEST);
            write_atomic(&path, format!("{text}{}", crate::notices::XESS_NOTICE).as_bytes())?;
            path
        } else {
            notices.clone()
        };
        let readme = out.join("readme").join(name).join("README.txt");
        write_atomic(&readme, player_readme(*target, bin, version, git).as_bytes())?;
        let bundles = out.join("bundles").join(name);
        let from = match target {
            DemoTarget::Windows => args.from_windows.clone(),
            DemoTarget::Linux => args.from_linux.clone(),
        };
        let dist_args = dist::Args {
            target: Some(target.dist_target()),
            bins: vec![args.bin.clone()],
            profile: "shipping".to_string(),
            out: Some(bundles.clone()),
            from,
            overlay: Some(payload),
            git: Some(git.clone()),
            readme: Some(readme),
            notices: Some(bundle_notices),
            // The shipped demo: the agent protocol and every network crate compiled out (ADR-5419).
            no_default_features: true,
            features: Some(if with_xess { "demo,regia/xess" } else { "demo" }.to_string()),
            // Steam and `run-demo.cmd` start the demo; `run-<bin>.cmd` is a tester launcher.
            no_launcher: *target == DemoTarget::Windows,
            ..dist::Args::default()
        };
        println!("demo-package: {name}");
        // `dist::run` returns only on success (every failure is an error).
        dist::run(&dist_args, root)?;
        let plan = dist::plan(&dist_args, root, &root.join("target"), std::env::consts::OS)?;
        let bundle_dir = bundles.join(&plan.bundle);
        let problems = dist::verify(&bundle_dir)?;
        if !problems.is_empty() {
            bail!(
                "the {name} bundle fails dist --verify:\n  {}",
                problems.join("\n  ")
            );
        }
        // The depot content root is the bundle folder.
        let content = out.join("depot").join("content").join(name);
        copy_tree(&bundle_dir, &content)?;
        depots.push((*target, name));
        // The plain archive.
        let archive = plan.archive.context("the bundle has no archive")?;
        let ext = target.dist_target().archive_ext();
        let dest = out
            .join("archive")
            .join(format!("regia-demo-{version}-{name}.{ext}"));
        fs::create_dir_all(dest.parent().context("archive dir")?)?;
        fs::copy(&archive, &dest).with_context(|| format!("copying {}", archive.display()))?;
        archives.push(dest);
        built.push(json!({
            "target": name,
            "git": git,
            "bundle": plan.bundle,
            "archive": archives.last().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()),
            "xess": with_xess,
        }));
    }

    // Depot scripts.
    let scripts = out.join("depot").join("scripts");
    let mut depot_entries = Vec::new();
    for (target, name) in &depots {
        let id = depot_id(args.app_id, *target);
        let file = format!("depot_build_{id}.vdf");
        write_atomic(&scripts.join(&file), depot_build_vdf(&id, name).as_bytes())?;
        depot_entries.push((id, file));
    }
    let app_text = args
        .app_id
        .map_or_else(|| "REPLACE_WITH_APP_ID".to_string(), |a| a.to_string());
    write_atomic(
        &scripts.join(format!("app_build_{app_text}.vdf")),
        app_build_vdf(&app_text, &format!("Regia Engine demo {version}"), &depot_entries).as_bytes(),
    )?;
    write_atomic(&scripts.join("UPLOAD.txt"), upload_notes(args.app_id).as_bytes())?;
    fs::create_dir_all(out.join("depot").join("output"))?;
    write_atomic(&out.join("DECK-CHECK.md"), deck_check_md(version).as_bytes())?;
    write_atomic(
        &out.join("WINDOWS-CHECK.md"),
        windows_check_md(version).as_bytes(),
    )?;

    // Checksums over the plain archives, read back.
    let archive_dir = out.join("archive");
    let names: Vec<String> = archives
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let sums = checksum_files(&archive_dir, &names).map_err(|e| anyhow::anyhow!("{e}"))?;
    write_atomic(
        &archive_dir.join(CHECKSUMS_FILE),
        checksums_text(&sums).as_bytes(),
    )?;
    let problems = verify_checksums(&archive_dir);
    if !problems.is_empty() {
        bail!(
            "{CHECKSUMS_FILE} does not verify: {}",
            problems
                .iter()
                .map(|p| p.message.clone())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    // The final scan: no Direct Order 27 name in anything produced.
    let produced = list_files(&out)?;
    if let Some(bad) = produced.iter().find(|f| do27_marker(f).is_some()) {
        bail!("do27_content_path: the package holds {bad}");
    }
    let summary = json!({
        "kind": "regia_demo_package",
        "engine_version": version,
        "profile": "shipping",
        "panic": "abort",
        "app_id": args.app_id,
        "scene_files": report.files.len(),
        "targets": built,
        "files": produced.len(),
    });
    write_atomic(
        &out.join("demo-package.json"),
        format!("{}\n", serde_json::to_string_pretty(&summary)?).as_bytes(),
    )?;
    println!(
        "demo-package: {} (depot/, archive/, DECK-CHECK.md, WINDOWS-CHECK.md)",
        out.display()
    );
    verify_package(&out)
}

/// `--verify` reconciles the existing summary with all of the artifacts it names.
fn verify_package(dir: &Path) -> Result<ExitCode> {
    // Inspect the whole tree before opening any artifact: never follow a link or read a FIFO.
    let produced = list_files(dir)?;
    // Keep checksum damage a reported verification failure, as before, and avoid parsing
    // damaged archive bytes. A rehashed malformed archive still fails the deeper checks.
    let checksum_problems = verify_checksums(&dir.join("archive"));
    if !checksum_problems.is_empty() {
        for problem in checksum_problems {
            eprintln!("error: {}: {}", problem.code, problem.message);
        }
        return Ok(ExitCode::FAILURE);
    }
    let mut problems = Vec::new();
    if let Some(path) = produced.iter().find(|path| do27_marker(path).is_some()) {
        problems.push(format!("do27_content_path: the package holds {path}"));
    }
    let summary_bytes = fs::read(dir.join("demo-package.json"))
        .context("demo_package_invalid: cannot read demo-package.json")?;
    let summary: Value =
        serde_json::from_slice(&summary_bytes).context("demo_package_invalid: invalid demo-package.json")?;
    let version = summary["engine_version"]
        .as_str()
        .context("demo_package_invalid: no engine_version")?;
    // The version contributes to paths. Require the version writer's literal numeric spelling.
    let parsed = regia_core::engine_version::EngineVersion::parse(version)
        .context("demo_package_invalid: invalid engine_version")?;
    if parsed.to_string() != version
        || summary["kind"] != "regia_demo_package"
        || summary["profile"] != "shipping"
        || summary["panic"] != "abort"
    {
        bail!("demo_package_invalid: expected a shipping, panic=abort demo package");
    }
    if summary["files"].as_u64() != Some(produced.len().saturating_sub(1) as u64) {
        problems.push("demo_package_inventory: file count differs from demo-package.json".into());
    }
    let targets = summary["targets"]
        .as_array()
        .filter(|targets| !targets.is_empty())
        .context("demo_package_invalid: no targets")?;
    let app_id = if summary["app_id"].is_null() {
        None
    } else {
        Some(
            summary["app_id"]
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .context("demo_package_invalid: invalid app_id")?,
        )
    };
    let mut seen = BTreeSet::new();
    let mut archives = BTreeSet::new();
    let mut depots = Vec::new();
    for row in targets {
        let target = match row["target"].as_str() {
            Some("windows") => DemoTarget::Windows,
            Some("linux") => DemoTarget::Linux,
            _ => bail!("demo_package_invalid: unknown target"),
        };
        if !seen.insert(target) {
            bail!("demo_package_invalid: duplicate target {}", target.name());
        }
        let name = target.name();
        let dist_target = target.dist_target();
        let bundle = format!("regia-{}-{version}", dist_target.name());
        let archive = format!("regia-demo-{version}-{name}.{}", dist_target.archive_ext());
        if row["bundle"] != bundle || row["archive"] != archive {
            bail!("demo_package_invalid: unexpected bundle/archive name for {name}");
        }
        archives.insert(archive.clone());
        let bundle_dir = dir.join("bundles").join(name).join(&bundle);
        let depot_dir = dir.join("depot/content").join(name);
        let manifest_bytes = fs::read(bundle_dir.join(dist::MANIFEST_FILE))
            .with_context(|| format!("demo_package_invalid: cannot read the {name} bundle manifest"))?;
        let manifest: Value = serde_json::from_slice(&manifest_bytes)
            .context("demo_package_invalid: invalid bundle manifest")?;
        if manifest["profile"] != "shipping"
            || manifest["engine_version"] != version
            || manifest["target"] != dist_target.name()
            || manifest["bundle"] != bundle
            || manifest["binaries"].as_array().is_none_or(Vec::is_empty)
        {
            bail!("demo_package_invalid: {name} is not a shipping bundle with a binary");
        }
        // ADR-5002: the depot names its build, and holds only what players need.
        let git = manifest["git"].as_str().unwrap_or_default();
        if git.is_empty() || git == "unknown" {
            problems.push(format!(
                "demo_package_git_unknown: the {name} bundle records no build id"
            ));
        }
        if let Some(recorded) = row["git"].as_str()
            && recorded != git
        {
            problems.push(format!(
                "demo_package_git_mismatch: the {name} bundle says {git}, demo-package.json says {recorded}"
            ));
        }
        for path in manifest["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["path"].as_str())
        {
            if path.starts_with("run-") && path.ends_with(".cmd") && path != "run-demo.cmd" {
                problems.push(format!(
                    "demo_package_extra_launcher: the {name} bundle ships {path}; players start run-demo.cmd"
                ));
            }
        }
        let readme = fs::read_to_string(bundle_dir.join("README.txt")).unwrap_or_default();
        if !readme.contains(&format!("Build {git}\n")) {
            problems.push(format!(
                "demo_package_readme: the {name} README.txt is not the player README of build {git}"
            ));
        }
        for path in [
            bundle_dir.clone(),
            depot_dir.clone(),
            dir.join("archive").join(&archive),
            dir.join("bundles")
                .join(name)
                .join(format!("{bundle}.{}", dist_target.archive_ext())),
        ] {
            problems.extend(dist::verify_against(&path, Some(&manifest))?);
        }
        let xess = row["xess"].as_bool().unwrap_or(false);
        if xess && target != DemoTarget::Windows {
            problems.push(format!("demo_package_xess: the {name} target records XeSS"));
        }
        for path in [&bundle_dir, &depot_dir] {
            problems.extend(legal_problems(name, path));
            problems.extend(xess_problems(name, path, xess));
            let content = check_content(&path.join(SCENE_DEST))?;
            problems.extend(content.problems);
            if summary["scene_files"].as_u64() != Some(content.files.len() as u64) {
                problems.push(format!("demo_package_inventory: {name} scene file count differs"));
            }
        }
        let id = depot_id(app_id, target);
        let file = format!("depot_build_{id}.vdf");
        check_package_text(
            &dir.join("depot/scripts").join(&file),
            &depot_build_vdf(&id, name),
            &mut problems,
        )?;
        depots.push((id, file));
    }
    // Enumeration and checksums must both cover exactly the summary's archive inventory.
    let archive_dir = dir.join("archive");
    let actual: BTreeSet<String> = list_files(&archive_dir)?
        .into_iter()
        .filter(|name| name != CHECKSUMS_FILE)
        .collect();
    if actual != archives {
        problems.push("demo_package_inventory: unlisted or missing archive".into());
    }

    let checksums = fs::read_to_string(archive_dir.join(CHECKSUMS_FILE))?;
    let listed: BTreeSet<String> = checksums
        .lines()
        .filter_map(|line| line.split_once("  ").map(|(_, path)| path.to_string()))
        .collect();
    if listed != archives {
        problems.push("demo_package_inventory: checksum list differs from target archives".into());
    }
    let app = app_id.map_or_else(|| "REPLACE_WITH_APP_ID".to_string(), |id| id.to_string());
    check_package_text(
        &dir.join("depot/scripts").join(format!("app_build_{app}.vdf")),
        &app_build_vdf(&app, &format!("Regia Engine demo {version}"), &depots),
        &mut problems,
    )?;
    if problems.is_empty() {
        println!("demo-package --verify {}: ok", dir.display());
        return Ok(ExitCode::SUCCESS);
    }
    for problem in &problems {
        eprintln!("error: {problem}");
    }
    Ok(ExitCode::FAILURE)
}

/// Checks generated depot text, including content roots and the empty `setlive` value.
fn check_package_text(path: &Path, expected: &str, problems: &mut Vec<String>) -> Result<()> {
    let actual =
        fs::read_to_string(path).with_context(|| format!("demo_package_script: {}", path.display()))?;
    if actual != expected {
        problems.push(format!(
            "demo_package_script: {} differs from the package's targets",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(files: &[(&str, &str)], manifest: Option<&str>) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir("demo_package_scene");
        for (rel, text) in files {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            fs::write(path, text).expect("write");
        }
        if let Some(manifest) = manifest {
            fs::write(dir.join(ASSET_MANIFEST), manifest).expect("manifest");
        }
        dir
    }

    const GOOD: &str = r#"{"format_version":1,"assets":[
        {"path":"scene.ron","title":"Scene","author":"Regia","licence":"LicenseRef-Regia-Sample","source":"generated"},
        {"path":"tex/a.png","title":"A","author":"Regia","licence":"CC0-1.0","source":"generated"}]}"#;

    fn codes(report: &ContentReport) -> Vec<&str> {
        report
            .problems
            .iter()
            .filter_map(|p| p.split(':').next())
            .collect()
    }

    #[test]
    fn a_licensed_scene_passes() {
        let dir = scene(&[("scene.ron", "x"), ("tex/a.png", "y")], Some(GOOD));
        let report = check_content(&dir).expect("check");
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert_eq!(report.files, ["scene.ron", "tex/a.png"]);
    }

    #[test]
    fn an_asset_without_a_row_or_a_licence_fails() {
        let manifest =
            r#"{"assets":[{"path":"scene.ron","licence":"MIT"},{"path":"tex/a.png","licence":"  "}]}"#;
        let dir = scene(
            &[("scene.ron", "x"), ("tex/a.png", "y"), ("extra.bin", "z")],
            Some(manifest),
        );
        let report = check_content(&dir).expect("check");
        assert_eq!(codes(&report), ["asset_no_licence", "asset_no_licence"]);
        assert!(report.problems[0].contains("tex/a.png"));
        assert!(report.problems[1].contains("extra.bin"));
        let tbd = r#"{"assets":[{"path":"scene.ron","licence":"TBD"}]}"#;
        let dir = scene(&[("scene.ron", "x")], Some(tbd));
        assert_eq!(codes(&check_content(&dir).expect("check")), ["asset_no_licence"]);
    }

    #[test]
    fn a_missing_or_broken_manifest_fails() {
        let dir = scene(&[("scene.ron", "x")], None);
        assert_eq!(
            codes(&check_content(&dir).expect("check")),
            ["asset_manifest_missing"]
        );
        let dir = scene(&[("scene.ron", "x")], Some("{not json"));
        assert_eq!(
            codes(&check_content(&dir).expect("check")),
            ["asset_manifest_invalid"]
        );
        let dir = scene(&[("scene.ron", "x")], Some(r#"{"assets":[{"title":"no path"}]}"#));
        assert_eq!(
            codes(&check_content(&dir).expect("check")),
            ["asset_manifest_invalid"]
        );
    }

    #[test]
    fn a_row_for_a_missing_file_and_a_duplicate_row_fail() {
        let manifest = r#"{"assets":[{"path":"a","licence":"MIT"},{"path":"a","licence":"MIT"},{"path":"gone","licence":"MIT"}]}"#;
        let dir = scene(&[("a", "x")], Some(manifest));
        assert_eq!(
            codes(&check_content(&dir).expect("check")),
            ["asset_row_duplicate", "asset_row_orphan"]
        );
    }

    #[test]
    fn direct_order_27_content_fails_by_file_name_row_text_and_scene_path() {
        for marker in ["DO27", "do-27", "Direct Order 27", "direct_order", "Direct.Order"] {
            assert!(do27_marker(marker).is_some(), "{marker}");
        }
        for fine in ["forest", "tex/rock_01.png", "Order-form", "undo"] {
            assert_eq!(do27_marker(fine), None, "{fine}");
        }
        // A file name.
        let manifest = r#"{"assets":[{"path":"maps/do27_valley.ron","licence":"MIT"}]}"#;
        let dir = scene(&[("maps/do27_valley.ron", "x")], Some(manifest));
        assert!(codes(&check_content(&dir).expect("check")).contains(&"do27_content_path"));
        // A manifest row's source text.
        let manifest =
            r#"{"assets":[{"path":"a","licence":"MIT","source":"exported from Direct Order 27"}]}"#;
        let dir = scene(&[("a", "x")], Some(manifest));
        let report = check_content(&dir).expect("check");
        assert_eq!(codes(&report), ["do27_content_path"]);
        assert!(report.problems[0].contains("row for a"));
        // The scene path itself.
        let base = scene(&[], None);
        let inner = base.join("DO27_assets");
        fs::create_dir_all(&inner).expect("dir");
        fs::write(inner.join("a"), "x").expect("write");
        fs::write(
            inner.join(ASSET_MANIFEST),
            r#"{"assets":[{"path":"a","licence":"MIT"}]}"#,
        )
        .expect("manifest");
        assert_eq!(
            codes(&check_content(&inner).expect("check")),
            ["do27_content_path"]
        );
    }

    #[test]
    fn the_shipping_profile_must_abort_on_panic() {
        let ok = "[profile.shipping]\ninherits = \"release\"\npanic = \"abort\"\n";
        assert!(check_profile_text(ok).is_ok());
        let unwind = "[profile.shipping]\ninherits = \"release\"\n";
        assert!(
            check_profile_text(unwind)
                .expect_err("must fail")
                .to_string()
                .contains("shipping_not_abort")
        );
        assert!(
            check_profile_text("[profile.release]\n")
                .expect_err("must fail")
                .to_string()
                .contains("shipping_profile_missing")
        );
    }

    #[test]
    fn this_workspace_ships_with_panic_abort() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        check_profile(&root).expect("the workspace shipping profile aborts");
    }

    #[test]
    fn vdf_scripts_follow_the_steamcmd_shapes_and_number_depots_from_the_app() {
        assert_eq!(depot_id(Some(1000), DemoTarget::Windows), "1001");
        assert_eq!(depot_id(Some(1000), DemoTarget::Linux), "1002");
        assert_eq!(depot_id(None, DemoTarget::Linux), "REPLACE_WITH_LINUX_DEPOT_ID");
        let app = app_build_vdf(
            "1000",
            "demo \"x\"",
            &[("1001".into(), "depot_build_1001.vdf".into())],
        );
        assert!(app.starts_with("\"appbuild\"\n{\n  \"appid\" \"1000\"\n"));
        assert!(app.contains("\"desc\" \"demo \\\"x\\\"\""));
        assert!(app.contains("\"setlive\" \"\"\n"), "nothing goes live by itself");
        assert!(app.contains("\"1001\" \"depot_build_1001.vdf\""));
        let depot = depot_build_vdf("1001", "windows");
        assert!(depot.contains("\"DepotID\" \"1001\""));
        assert!(depot.contains("\"LocalPath\" \"windows/*\""));
        assert!(depot.contains("\"FileExclusion\" \"*.pdb\""));
    }

    #[test]
    fn launchers_start_the_demo_on_the_bundled_scene() {
        let sh = launcher_sh("showcase");
        assert!(sh.starts_with("#!/bin/sh\n"));
        assert!(sh.contains("exec \"./showcase\" --demo content/scene \"$@\""));
        let cmd = launcher_cmd("showcase");
        assert!(cmd.contains("\"%~dp0showcase.exe\" --demo content\\scene %*"));
        assert!(cmd.ends_with("\r\n") && !cmd.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn deck_steps_and_credits_are_written() {
        let md = deck_check_md("0.2.0");
        for needle in [
            "b3sum --check CHECKSUMS.b3",
            "SteamOS ships no `b3sum`",
            "sha256sum regia-demo-0.2.0-linux.tar",
            "./run-demo.sh --check",
            "Game Mode",
            "regia-demo-0.2.0-linux.tar",
        ] {
            assert!(md.contains(needle), "{needle}");
        }
        let rows = vec![json!({"path":"a.png","title":"A","author":"Me","licence":"CC0-1.0","source":"s"})];
        assert!(credits_text(&rows).contains("A (a.png) by Me: CC0-1.0 - s"));
        // MPL-2.0 § 3.2: the demo names where the engine's source is (owner decision 2026-11-20).
        assert!(
            credits_text(&rows)
                .contains("available at https://github.com/Regia-Engine/Regia-Engine (MPL-2.0")
        );
    }

    #[test]
    fn windows_steps_cover_the_hash_the_cold_launch_the_tier_and_the_quit() {
        let md = windows_check_md("0.2.0");
        for needle in [
            // Windows has no b3sum and no sha256sum: the built-in hashers, compared with the build host.
            "certutil -hashfile regia-demo-0.2.0-windows.zip SHA256",
            "sha256sum regia-demo-0.2.0-windows.zip",
            // A folder with spaces and non-ASCII letters, as players unpack to.
            "regia-windows-0.2.0",
            "run-demo.cmd --check",
            "echo %ERRORLEVEL%",
            // The first launch on a cold driver cache is long on the minimum PC (ADR-5578): say so, and that the
            // second launch is the comparison.
            "first launch",
            "second launch",
            // Minimum-spec selection: the picker names the tier and says why.
            "Low",
            "why",
            "%APPDATA%\\RegiaEngine\\showcase\\logs",
            "panic-",
            "VCRUNTIME140.dll",
            "run-demo.cmd --exit-after-frames 30",
        ] {
            assert!(md.contains(needle), "WINDOWS-CHECK.md lacks {needle:?}");
        }
        // Nothing here logs in or uploads.
        for word in ["steamcmd", "+login", "partner"] {
            assert!(!md.contains(word), "WINDOWS-CHECK.md mentions {word}");
        }
    }

    #[test]
    fn an_overlay_adds_files_to_the_dist_plan_and_refuses_a_collision() {
        let overlay = regia_test_utils::unique_temp_dir("demo_package_overlay");
        fs::create_dir_all(overlay.join("content/scene")).expect("dir");
        fs::write(overlay.join("content/scene/a.ron"), "x").expect("a");
        fs::write(overlay.join("run-demo.sh"), "#!/bin/sh\n").expect("sh");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let args = dist::Args {
            target: Some(Target::Deck),
            profile: "shipping".into(),
            overlay: Some(overlay.clone()),
            ..dist::Args::default()
        };
        let plan = dist::plan(&args, &root, &root.join("target"), "linux").expect("plan");
        let sh = plan
            .files
            .iter()
            .find(|f| f.dest == "run-demo.sh")
            .expect("launcher");
        assert!(sh.executable && sh.role == dist::Role::Overlay);
        assert!(
            plan.files
                .iter()
                .any(|f| f.dest == "content/scene/a.ron" && !f.executable)
        );
        fs::write(overlay.join("README.txt"), "clash").expect("clash");
        let err = dist::plan(&args, &root, &root.join("target"), "linux").expect_err("must fail");
        assert!(err.to_string().contains("README.txt"), "{err}");
    }

    // Header-only PE/ELF fixtures for package integrity tests, NOT runnable shipping binaries.
    // Same public PE header layout exercised by xtask/tests/dist.rs (ADR-868).
    /// The fixture repository root (profile, licences, header-only binaries under `bin/`) and a scene.
    fn fixture_inputs() -> (PathBuf, PathBuf) {
        let root = regia_test_utils::unique_temp_dir("demo_package_fixture");
        write_atomic(
            &root.join("Cargo.toml"),
            b"[profile.shipping]\npanic = \"abort\"\n",
        )
        .expect("profile");
        for name in ["LICENSE", "THIRD_PARTY_NOTICES.md"] {
            write_atomic(&root.join(name), b"Test fixture license").expect("license");
        }
        write_atomic(
            &root.join(crate::demo_legal::EULA_PATH),
            b"# EULA draft fixture\n",
        )
        .expect("eula");
        write_atomic(
            &root.join(crate::demo_legal::NOTICES_PATH),
            format!("{DEMO_NOTICES_TITLE}\n\nfixture\n").as_bytes(),
        )
        .expect("notices");
        let mut pe = vec![0_u8; 512];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x40_u32.to_le_bytes());
        pe[0x40..0x44].copy_from_slice(b"PE\0\0");
        pe[0x44..0x46].copy_from_slice(&0x8664_u16.to_le_bytes());
        pe[0x54..0x56].copy_from_slice(&240_u16.to_le_bytes());
        pe[0x58..0x5a].copy_from_slice(&0x20b_u16.to_le_bytes());
        pe[0x9c..0x9e].copy_from_slice(&3_u16.to_le_bytes());
        write_atomic(&root.join("bin/showcase.exe"), &pe).expect("PE fixture");
        let mut elf = vec![0_u8; 128];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[0x12..0x14].copy_from_slice(&62_u16.to_le_bytes());
        elf[0x28..0x30].copy_from_slice(&64_u64.to_le_bytes());
        elf[0x3a..0x3c].copy_from_slice(&64_u16.to_le_bytes());
        elf[0x3c..0x3e].copy_from_slice(&1_u16.to_le_bytes());
        write_atomic(&root.join("bin/showcase"), &elf).expect("ELF fixture");
        let scene = scene(&[("scene.ron", "x"), ("tex/a.png", "y")], Some(GOOD));
        (root, scene)
    }

    /// Package arguments for [`fixture_inputs`]: both targets from `bin/`, each with its build id
    /// (the two ids differ, as the real v0.2.20 inputs do).
    fn fixture_args(root: &Path, scene: &Path) -> Args {
        Args {
            scene: scene.to_path_buf(),
            out: Some(root.join("package")),
            targets: vec![DemoTarget::Windows, DemoTarget::Linux],
            bin: "showcase".into(),
            app_id: None,
            from_windows: Some(root.join("bin")),
            from_linux: Some(root.join("bin")),
            git_windows: Some("b394124ea".into()),
            git_linux: Some("924493f56".into()),
            check: false,
            verify: None,
            xess_sdk: None,
        }
    }

    fn staged_test_package() -> PathBuf {
        let (root, scene) = fixture_inputs();
        let args = fixture_args(&root, &scene);
        assert_eq!(run(&args, &root).expect("stage fixture"), ExitCode::SUCCESS);
        fs::remove_dir_all(scene).expect("clean scene");
        root.join("package")
    }

    fn assert_package_rejected(dir: &Path) {
        assert!(
            !matches!(verify_package(dir), Ok(code) if code == ExitCode::SUCCESS),
            "incomplete or altered package must be refused: {}",
            dir.display()
        );
    }

    /// An XeSS SDK fixture: the SR runtime, the other features' DLLs and both terms files.
    fn xess_sdk_fixture(root: &Path) -> PathBuf {
        let sdk = root.join("xess-sdk");
        write_atomic(&sdk.join("bin/libxess.dll"), b"sr runtime").expect("runtime");
        write_atomic(&sdk.join("bin/libxess_fg.dll"), b"not staged").expect("fg");
        write_atomic(&sdk.join("LICENSE.txt"), b"intel terms").expect("terms");
        write_atomic(&sdk.join("third-party-programs.txt"), b"programs").expect("programs");
        sdk
    }

    /// ADR-7820: `--xess-sdk` puts Intel's runtime and both terms files beside the Windows binary, in the
    /// bundle and the depot, adds the XeSS notices section there, leaves the Linux bundle untouched, and
    /// `--verify` holds the package to it.
    #[test]
    fn demo_package_xess_stages_the_windows_runtime_terms_and_notices() {
        let (root, scene) = fixture_inputs();
        let sdk = xess_sdk_fixture(&root);
        let mut args = fixture_args(&root, &scene);
        args.xess_sdk = Some(sdk);
        assert_eq!(run(&args, &root).expect("stage fixture"), ExitCode::SUCCESS);
        fs::remove_dir_all(&scene).expect("clean scene");
        let out = root.join("package");
        let windows = out.join("depot/content/windows");
        assert_eq!(
            fs::read(windows.join("libxess.dll")).expect("runtime"),
            b"sr runtime"
        );
        assert_eq!(
            fs::read(windows.join(XESS_FILES[1])).expect("terms"),
            b"intel terms"
        );
        assert_eq!(
            fs::read(windows.join(XESS_FILES[2])).expect("programs"),
            b"programs"
        );
        assert!(!windows.join("libxess_fg.dll").exists());
        let notices = fs::read_to_string(windows.join(NOTICES_DEST)).expect("notices");
        assert!(notices.starts_with(DEMO_NOTICES_TITLE), "{notices}");
        assert!(
            notices.contains("Optional Intel XeSS Super Resolution runtime"),
            "{notices}"
        );
        let linux = out.join("depot/content/linux");
        assert!(XESS_FILES.iter().all(|f| !linux.join(f).exists()));
        assert!(xess_problems("windows", &windows, true).is_empty());
        assert!(xess_problems("linux", &linux, false).is_empty());
        // A Windows depot without Intel's terms is refused (the problem list names it).
        fs::remove_file(windows.join(XESS_FILES[1])).expect("remove terms");
        let problems = xess_problems("windows", &windows, true);
        assert!(problems.iter().any(|p| p.contains(XESS_FILES[1])), "{problems:?}");
        assert_package_rejected(&out);
        fs::remove_dir_all(&root).expect("cleanup");

        // Without the opt-in a runtime in a bundle is a problem too.
        let plain = regia_test_utils::unique_temp_dir("demo_package_xess_plain");
        write_atomic(&plain.join("libxess.dll"), b"sr runtime").expect("stray runtime");
        let problems = xess_problems("windows", &plain, false);
        assert!(
            problems.iter().any(|p| p.contains("without --xess-sdk")),
            "{problems:?}"
        );
        fs::remove_dir_all(plain).expect("cleanup");
    }

    #[test]
    fn demo_package_xess_refuses_a_linux_only_package_and_an_incomplete_sdk() {
        let (root, scene) = fixture_inputs();
        let sdk = xess_sdk_fixture(&root);
        let mut args = fixture_args(&root, &scene);
        args.xess_sdk = Some(sdk.clone());
        args.targets = vec![DemoTarget::Linux];
        let error = run(&args, &root).expect_err("Windows only");
        assert!(error.to_string().starts_with("xess_package_invalid"), "{error:#}");
        args.targets = vec![DemoTarget::Windows, DemoTarget::Linux];
        fs::write(sdk.join("third-party-programs.txt"), b"").expect("empty programs");
        let error = run(&args, &root).expect_err("empty terms file");
        assert!(error.to_string().starts_with("xess_package_invalid"), "{error:#}");
        assert!(xess_sources(Path::new("relative/sdk")).is_err());
        fs::remove_dir_all(&root).expect("cleanup");
        fs::remove_dir_all(&scene).expect("clean scene");
    }

    #[test]
    fn demo_package_requires_summary_bundles_depots_and_nonempty_checksums() {
        for missing in ["demo-package.json", "bundles/windows", "depot/content/windows"] {
            let out = staged_test_package();
            assert_eq!(verify_package(&out).expect("valid fixture"), ExitCode::SUCCESS);
            let path = out.join(missing);
            if path.is_dir() {
                fs::remove_dir_all(path).expect("remove folder");
            } else {
                fs::remove_file(path).expect("remove file");
            }
            assert_package_rejected(&out);
            if missing != "depot/content/windows" {
                let error = verify_package(&out).expect_err("missing required package metadata");
                assert!(error.to_string().contains("demo_package_invalid"), "{error:#}");
            }
            fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
        }
        let out = staged_test_package();
        write_atomic(&out.join("archive").join(CHECKSUMS_FILE), b"").expect("empty list");
        assert_package_rejected(&out);
        fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
    }

    #[test]
    fn demo_package_rejects_unlisted_archives_and_a_rehashed_non_archive() {
        let out = staged_test_package();
        let archive = out.join("archive");
        write_atomic(&archive.join("extra.zip"), b"unlisted").expect("extra");
        assert_package_rejected(&out);
        fs::remove_file(archive.join("extra.zip")).expect("remove extra");
        let name = format!("regia-demo-{}-windows.zip", dist::engine_version());
        write_atomic(&archive.join(&name), b"not an archive").expect("replace");
        let sums = checksum_files(&archive, &[name]).expect("rehash");
        write_atomic(&archive.join(CHECKSUMS_FILE), checksums_text(&sums).as_bytes()).expect("list");
        assert_package_rejected(&out);
        fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
    }

    #[test]
    fn demo_package_rechecks_shipping_profile_and_depot_scripts() {
        for mutation in ["profile", "script"] {
            let out = staged_test_package();
            if mutation == "profile" {
                let path = out.join("demo-package.json");
                let mut summary: Value =
                    serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
                summary["panic"] = json!("unwind");
                write_atomic(&path, &serde_json::to_vec(&summary).expect("json")).expect("summary");
            } else {
                write_atomic(
                    &out.join("depot/scripts/depot_build_REPLACE_WITH_WINDOWS_DEPOT_ID.vdf"),
                    b"changed root",
                )
                .expect("script");
            }
            assert_package_rejected(&out);
            fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
        }
    }

    #[test]
    fn demo_package_checks_every_archive_row_and_the_deck_unpack_directory() {
        let out = staged_test_package();
        let path = out.join("archive").join(CHECKSUMS_FILE);
        let text = fs::read_to_string(&path).expect("list");
        let first = text.lines().next().expect("first");
        write_atomic(&path, format!("{first}\n").as_bytes()).expect("truncate inventory");
        assert_package_rejected(&out);
        let instructions = deck_check_md(dist::engine_version());
        assert!(instructions.contains(&format!("cd regia-deck-{}", dist::engine_version())));
        // The Windows steps sit next to the Deck's and name the Windows bundle folder.
        let windows = fs::read_to_string(out.join("WINDOWS-CHECK.md")).expect("WINDOWS-CHECK.md");
        assert_eq!(windows, windows_check_md(dist::engine_version()));
        assert!(windows.contains(&format!("regia-windows-{}", dist::engine_version())));
        fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
    }

    #[test]
    fn demo_package_rejects_extra_scene_files_and_changed_asset_bytes() {
        let out = staged_test_package();
        let scene = out.join("depot/content/windows").join(SCENE_DEST);
        write_atomic(&scene.join("unlisted.bin"), b"unlisted asset").expect("extra");
        assert_package_rejected(&out);
        fs::remove_file(scene.join("unlisted.bin")).expect("restore");
        write_atomic(&scene.join("tex/a.png"), b"changed").expect("tamper");
        assert_package_rejected(&out);
        fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn demo_package_refuses_links_before_opening_artifacts() {
        let out = staged_test_package();
        std::os::unix::fs::symlink("missing", out.join("archive/link.zip")).expect("link");
        let error = verify_package(&out).expect_err("refuse link without following it");
        assert!(
            error.to_string().contains("demo_package_special_file"),
            "{error:#}"
        );
        fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
    }

    #[test]
    fn demo_package_refuses_duplicate_targets_and_mismatched_copy_manifests() {
        for mutation in ["target", "manifest"] {
            let out = staged_test_package();
            let path = if mutation == "target" {
                out.join("demo-package.json")
            } else {
                out.join("depot/content/windows").join(dist::MANIFEST_FILE)
            };
            let mut value: Value = serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
            if mutation == "target" {
                value["targets"] = json!([value["targets"][0], value["targets"][0]]);
            } else {
                value["git"] = json!("different-build");
            }
            write_atomic(&path, &serde_json::to_vec(&value).expect("json")).expect("mutate");
            assert_package_rejected(&out);
            fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
        }
    }

    #[test]
    fn the_checked_in_demo_scene_is_cleared_and_has_working_credits() {
        let scene = Path::new(env!("CARGO_MANIFEST_DIR")).join("../samples/showcase/example_scene");
        let report = check_content(&scene).expect("content check");
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert!(!report.files.is_empty());
        let credits = showcase::demo::credits::Credits::load(&scene);
        assert!(credits.error.is_none(), "{:?}", credits.error);
        assert!(credits.unlisted.is_empty(), "{:?}", credits.unlisted);
    }

    #[test]
    fn the_depot_and_archives_verify_after_a_staged_run() {
        let out = staged_test_package();
        assert_eq!(verify_package(&out).expect("verify"), ExitCode::SUCCESS);
        let archive = out
            .join("archive")
            .join(format!("regia-demo-{}-windows.zip", dist::engine_version()));
        write_atomic(&archive, b"changed archive").expect("tamper");
        assert_eq!(
            verify_package(&out).expect("checksum failure is reported"),
            ExitCode::FAILURE
        );
        fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
    }

    #[test]
    fn prebuilt_binaries_need_their_build_id_before_anything_is_written() {
        // ADR-5002, audit row 6: `git unknown` (or the repository's HEAD) is not the build of
        // binaries made elsewhere.
        let (root, scene) = fixture_inputs();
        for missing in [DemoTarget::Windows, DemoTarget::Linux] {
            let mut args = fixture_args(&root, &scene);
            match missing {
                DemoTarget::Windows => args.git_windows = None,
                DemoTarget::Linux => args.git_linux = None,
            }
            let err = run(&args, &root).expect_err("no build id");
            assert!(
                err.to_string().starts_with("demo_package_git_unknown")
                    && err.to_string().contains(&format!("--git-{}", missing.name())),
                "{err}"
            );
            assert!(!root.join("package").exists(), "nothing written");
        }
        let mut args = fixture_args(&root, &scene);
        args.git_linux = Some("two words".into());
        let err = run(&args, &root).expect_err("not a revision");
        assert!(err.to_string().starts_with("dist_git_invalid"), "{err}");
        fs::remove_dir_all(root).expect("cleanup");
        fs::remove_dir_all(scene).expect("cleanup");
    }

    #[test]
    fn the_player_readme_names_the_build_and_holds_no_developer_steps() {
        for (target, start) in [
            (DemoTarget::Windows, "double-click run-demo.cmd"),
            (DemoTarget::Linux, "run ./run-demo.sh"),
        ] {
            let text = player_readme(target, "showcase", "0.2.20", "b394124ea");
            assert!(
                text.starts_with(&format!("{} 0.2.20 for ", crate::steam_demo_upload::demo_name())),
                "{text}"
            );
            assert!(text.contains("\nBuild b394124ea\n"), "{text}");
            assert!(text.contains(start), "{text}");
            // The first start on a weak PC is long (79 s on the Intel N150, ADR-5578): the player is told to wait.
            assert!(
                text.contains("The first start prepares the graphics for your PC"),
                "{text}"
            );
            assert!(text.contains("later starts take a few seconds"), "{text}");
            assert!(text.contains("no\n  telemetry and no crash reports"), "{text}");
            // The shipped demo has no agent endpoint (ADR-5419; on the Deck it held 0 sockets, ADR-5452).
            assert!(text.contains("it opens no network port"), "{text}");
            assert!(
                !text.contains("127.0.0.1") && !text.contains("local port"),
                "{text}"
            );
            // The demo's own help line, wrapped.
            assert!(text.contains("F12 screenshot"), "{text}");
            assert!(text.contains("RegiaEngine") && text.contains("showcase") && !text.contains("<exe>"));
            for developer in [
                "test bundle",
                "cargo",
                "repository",
                "REGIA_NATIVE_CRASH",
                "unknown",
            ] {
                assert!(!text.contains(developer), "{target:?}: {developer:?} in {text}");
            }
            assert!(text.lines().all(|l| l.chars().count() <= 100), "{text}");
        }
        assert_eq!(wrap("aa bb cc", 7, "  "), "  aa bb\n  cc\n");
        assert_eq!(wrap("", 10, "  "), "");
    }

    #[test]
    fn the_depot_holds_the_player_readme_its_build_ids_and_no_tester_launcher() {
        // Audit rows 5 and 6.
        let out = staged_test_package();
        let summary: Value =
            serde_json::from_slice(&fs::read(out.join("demo-package.json")).expect("summary")).expect("json");
        for (row, (name, git)) in summary["targets"]
            .as_array()
            .expect("targets")
            .iter()
            .zip([("windows", "b394124ea"), ("linux", "924493f56")])
        {
            assert_eq!(row["target"], name);
            assert_eq!(row["git"], git);
            let depot = out.join("depot/content").join(name);
            let manifest: Value =
                serde_json::from_slice(&fs::read(depot.join(dist::MANIFEST_FILE)).expect("manifest"))
                    .expect("json");
            assert_eq!(manifest["git"], git);
            let readme = fs::read_to_string(depot.join("README.txt")).expect("readme");
            assert!(readme.contains(&format!("Build {git}\n")), "{readme}");
            assert!(!readme.contains("test bundle"), "{readme}");
            assert!(!depot.join("DEMO-README.txt").exists());
        }
        let windows = out.join("depot/content/windows");
        assert!(windows.join("run-demo.cmd").is_file());
        assert!(!windows.join("run-showcase.cmd").exists(), "no tester launcher");
        assert_eq!(verify_package(&out).expect("verify"), ExitCode::SUCCESS);
        fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
    }

    #[test]
    fn verify_refuses_an_unknown_or_changed_build_id() {
        let out = staged_test_package();
        let path = out.join("demo-package.json");
        let mut summary: Value = serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
        summary["targets"][1]["git"] = json!("another-build");
        write_atomic(&path, &serde_json::to_vec(&summary).expect("json")).expect("summary");
        assert_eq!(
            verify_package(&out).expect("reported"),
            ExitCode::FAILURE,
            "the summary and the bundle disagree on the Linux build"
        );
        fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
    }

    /// ADR-5419: both depots carry the EULA and the demo's own notices (not the engine-wide list), and
    /// `--verify` checks they are there.
    #[test]
    fn both_depots_ship_the_eula_and_the_demo_notices() {
        let out = staged_test_package();
        for name in ["windows", "linux"] {
            let depot = out.join("depot/content").join(name);
            let eula = fs::read_to_string(depot.join(EULA_DEST)).expect("eula");
            assert_eq!(
                eula, "# EULA draft fixture\n",
                "{name}: the draft until docs/demo/EULA.md lands"
            );
            let notices = fs::read_to_string(depot.join(NOTICES_DEST)).expect("notices");
            assert!(notices.starts_with(DEMO_NOTICES_TITLE), "{name}: {notices}");
            assert!(legal_problems(name, &depot).is_empty());
            assert!(
                fs::read_to_string(depot.join("licenses/LICENSE")).is_ok(),
                "{name}"
            );
        }
        assert_eq!(verify_package(&out).expect("verify"), ExitCode::SUCCESS);
        fs::remove_dir_all(out.parent().expect("fixture root")).expect("cleanup");
    }

    #[test]
    fn a_folder_without_the_eula_or_with_the_engine_notices_is_reported() {
        let dir = regia_test_utils::unique_temp_dir("demo_package_legal");
        let problems = legal_problems("windows", &dir);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().all(|p| p.starts_with("demo_package_legal:")));
        write_atomic(&dir.join(EULA_DEST), b"terms\n").expect("eula");
        write_atomic(&dir.join(NOTICES_DEST), b"# Third-party notices\n\nengine-wide\n").expect("notices");
        let problems = legal_problems("windows", &dir);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("demo notices"), "{problems:?}");
        write_atomic(
            &dir.join(NOTICES_DEST),
            format!("{DEMO_NOTICES_TITLE}\n").as_bytes(),
        )
        .expect("notices");
        assert!(legal_problems("windows", &dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_package_prefers_the_approved_eula_and_needs_both_legal_files() {
        let (root, scene) = fixture_inputs();
        let (eula, notices) = legal_sources(&root).expect("draft and notices");
        assert!(eula.ends_with(crate::demo_legal::EULA_PATH));
        assert!(notices.ends_with(crate::demo_legal::NOTICES_PATH));
        write_atomic(&root.join(EULA_FINAL_PATH), b"# EULA\n").expect("approved");
        assert!(
            legal_sources(&root)
                .expect("approved")
                .0
                .ends_with(EULA_FINAL_PATH)
        );
        fs::remove_file(root.join(crate::demo_legal::NOTICES_PATH)).expect("rm notices");
        let err = run(&fixture_args(&root, &scene), &root).expect_err("no notices");
        assert!(err.to_string().contains("demo_package_legal_missing"), "{err}");
        assert!(!root.join("package").exists(), "nothing is written");
        // The repository's own files are there.
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        assert!(legal_sources(&repo).is_ok());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&scene);
    }
}
