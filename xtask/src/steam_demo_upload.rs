// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask steam-demo-upload --app <id> --depot-win <id> --depot-linux <id> --dry-run`
//! (ADR-4899): stage a finished demo package (`cargo xtask demo-package`) into the steamcmd
//! content layout and print the exact upload command for the owner.
//!
//! ```text
//! <out>/                                  default target/steam-demo-upload
//!   content/windows/  content/linux/      the depot content roots, copied file by file and re-hashed
//!   scripts/app_build_<app>.vdf           the upload build (`Preview` 0)
//!   scripts/app_build_<app>_preview.vdf   the same build as a steamcmd preview (`Preview` 1, nothing uploaded)
//!   scripts/depot_build_<depot>.vdf       one per depot (Windows x64, Linux x64)
//!   output/                               steamcmd's build cache and logs
//!   store/                                the store page text, rendered with the demo's name
//!   UPLOAD-COMMAND.txt                    the two steamcmd commands, in order
//!   steam-upload-plan.json                what was staged: ids, files, bytes, hashes, commands
//! ```
//!
//! The command never runs steamcmd, never logs in and never reads or stores a credential: it
//! only prints the command with a `<your-steam-build-account>` placeholder. `--dry-run` is
//! required so the invocation says what it does. Every package problem is reported with a
//! stable code (`steam_*`) and nothing is staged.
//!
//! The templates live in the repository's `steamworks/` folder (VDF and text, exempt from the
//! Rust-only rule) and are compiled in. The demo's working name is the single line of
//! `steamworks/DEMO_NAME.txt`: a rename is one edit there.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::dist;

const DEMO_NAME_FILE: &str = include_str!("../../steamworks/DEMO_NAME.txt");

/// `steamworks/app_build.vdf.in`: the app build script template.
pub const APP_BUILD_TEMPLATE: &str = include_str!("../../steamworks/app_build.vdf.in");
/// `steamworks/depot_build_windows.vdf.in`: the Windows x64 depot template.
pub const DEPOT_WINDOWS_TEMPLATE: &str = include_str!("../../steamworks/depot_build_windows.vdf.in");
/// `steamworks/depot_build_linux.vdf.in`: the Linux x64 (and Steam Deck) depot template.
pub const DEPOT_LINUX_TEMPLATE: &str = include_str!("../../steamworks/depot_build_linux.vdf.in");
/// `steamworks/store/short-description.txt.in`: the store page's short description.
pub const SHORT_DESCRIPTION_TEMPLATE: &str = include_str!("../../steamworks/store/short-description.txt.in");
/// `steamworks/store/about.bbcode.in`: the store page's "About this game" text (Steam markup).
pub const ABOUT_TEMPLATE: &str = include_str!("../../steamworks/store/about.bbcode.in");
/// `steamworks/store/system-requirements.txt.in`: minimum and recommended system requirements.
pub const SYSTEM_REQUIREMENTS_TEMPLATE: &str =
    include_str!("../../steamworks/store/system-requirements.txt.in");

/// The plan file written last into a finished staging folder.
pub const PLAN_FILE: &str = "steam-upload-plan.json";
/// The `kind` of [`PLAN_FILE`].
pub const PLAN_KIND: &str = "regia_steam_demo_upload";
/// Present while a staging folder is being written; a folder with it may be replaced.
pub const STAGING_MARKER: &str = ".steam-demo-upload-staging";
/// The `kind` of the demo package summary (`demo-package.json`) this command reads.
pub const PACKAGE_KIND: &str = "regia_demo_package";
/// The longest short description the store page takes, in characters.
pub const SHORT_DESCRIPTION_MAX: usize = 300;
/// The login placeholder in the printed command; the owner types the account name.
pub const LOGIN_PLACEHOLDER: &str = "<your-steam-build-account>";

/// The demo's working name, from `steamworks/DEMO_NAME.txt`.
#[must_use]
pub fn demo_name() -> &'static str {
    DEMO_NAME_FILE.trim()
}

/// One store graphic the owner uploads on the partner site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreAsset {
    /// File name in the kit folder (`<scratch>`).
    pub file: &'static str,
    /// What the partner site calls it.
    pub name: &'static str,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Whether the store page checklist needs it before review.
    pub required: bool,
}

/// The store graphics with their sizes (steamworks/STEAM-DEMO-KIT.md § Graphics lists the same
/// rows; a test keeps the two in step). The file extension is the format Valve asks for.
pub const STORE_ASSETS: &[StoreAsset] = &[
    StoreAsset {
        file: "header-capsule.png",
        name: "Header capsule",
        width: 920,
        height: 430,
        required: true,
    },
    StoreAsset {
        file: "small-capsule.png",
        name: "Small capsule",
        width: 462,
        height: 174,
        required: true,
    },
    StoreAsset {
        file: "main-capsule.png",
        name: "Main capsule",
        width: 1232,
        height: 706,
        required: true,
    },
    StoreAsset {
        file: "vertical-capsule.png",
        name: "Vertical capsule",
        width: 748,
        height: 896,
        required: true,
    },
    StoreAsset {
        file: "page-background.png",
        name: "Page background",
        width: 1438,
        height: 810,
        required: false,
    },
    StoreAsset {
        file: "library-capsule.png",
        name: "Library capsule",
        width: 600,
        height: 900,
        required: true,
    },
    StoreAsset {
        file: "library-header.png",
        name: "Library header",
        width: 920,
        height: 430,
        required: true,
    },
    StoreAsset {
        file: "library-hero.png",
        name: "Library hero",
        width: 3840,
        height: 1240,
        required: true,
    },
    StoreAsset {
        file: "library-logo.png",
        name: "Library logo",
        width: 1280,
        height: 720,
        required: true,
    },
    StoreAsset {
        file: "app-icon.jpg",
        name: "App icon (JPG)",
        width: 184,
        height: 184,
        required: true,
    },
    StoreAsset {
        file: "shortcut-icon.png",
        name: "Shortcut icon (PNG)",
        width: 512,
        height: 512,
        required: true,
    },
    StoreAsset {
        file: "screenshot-01.png",
        name: "Screenshot (5 or more)",
        width: 1920,
        height: 1080,
        required: true,
    },
];

/// Command-line options of `cargo xtask steam-demo-upload`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// The Steam app id the owner created on the partner site.
    #[arg(long = "app", value_name = "ID")]
    pub app: String,
    /// The Windows x64 depot id.
    #[arg(long = "depot-win", value_name = "ID")]
    pub depot_win: String,
    /// The Linux x64 depot id (also what the Steam Deck installs).
    #[arg(long = "depot-linux", value_name = "ID")]
    pub depot_linux: String,
    /// The finished demo package (default `target/demo-package`).
    #[arg(long, value_name = "DIR")]
    pub package: Option<PathBuf>,
    /// Where to stage the upload (default `target/steam-demo-upload`).
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Stage and print the steamcmd command; never run it. Required.
    #[arg(long)]
    pub dry_run: bool,
    /// Print the result (or the problems) as JSON on stdout.
    #[arg(long)]
    pub json: bool,
}

/// One problem with a stable code (`steam_*`, documented in book/agents/steam-demo-upload.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The stable error code.
    pub code: &'static str,
    /// What is wrong, in words.
    pub message: String,
}

impl Problem {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The three Steam ids, checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SteamIds {
    /// The app id.
    pub app: u32,
    /// The Windows x64 depot id.
    pub depot_windows: u32,
    /// The Linux x64 depot id.
    pub depot_linux: u32,
}

/// Parses and checks the ids: decimal, not zero, all three different.
///
/// # Errors
/// `steam_id_invalid` or `steam_id_duplicate` problems.
pub fn parse_ids(app: &str, depot_win: &str, depot_linux: &str) -> Result<SteamIds, Vec<Problem>> {
    let mut problems = Vec::new();
    let mut parse = |flag: &str, text: &str| -> u32 {
        match text.trim().parse::<u32>() {
            Ok(id) if id > 0 && text.trim().bytes().all(|b| b.is_ascii_digit()) => id,
            _ => {
                problems.push(Problem::new(
                    "steam_id_invalid",
                    format!("{flag} {text:?} is not a Steam id (a decimal number above 0)"),
                ));
                0
            }
        }
    };
    let ids = SteamIds {
        app: parse("--app", app),
        depot_windows: parse("--depot-win", depot_win),
        depot_linux: parse("--depot-linux", depot_linux),
    };
    if problems.is_empty() {
        let all = [ids.app, ids.depot_windows, ids.depot_linux];
        if all.iter().collect::<BTreeSet<_>>().len() != all.len() {
            problems.push(Problem::new(
                "steam_id_duplicate",
                format!(
                    "--app {}, --depot-win {} and --depot-linux {} must be three different ids",
                    ids.app, ids.depot_windows, ids.depot_linux
                ),
            ));
        }
    }
    if problems.is_empty() {
        Ok(ids)
    } else {
        Err(problems)
    }
}

/// A depot platform of the demo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Platform {
    /// Windows x64.
    Windows,
    /// Linux x64 (glibc 2.31 floor; the Steam Deck installs it).
    Linux,
}

impl Platform {
    /// Both platforms, in staging order.
    pub const ALL: [Self; 2] = [Self::Windows, Self::Linux];

    /// The folder name in `depot/content/` and in the staged `content/`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
        }
    }

    /// The `target` its `dist-manifest.json` must name.
    #[must_use]
    pub fn dist_target(self) -> dist::Target {
        match self {
            Self::Windows => dist::Target::Windows,
            Self::Linux => dist::Target::Deck,
        }
    }

    fn depot_id(self, ids: &SteamIds) -> u32 {
        match self {
            Self::Windows => ids.depot_windows,
            Self::Linux => ids.depot_linux,
        }
    }
}

/// One checked depot source of the package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepotSource {
    /// The platform.
    pub platform: Platform,
    /// `<package>/depot/content/<platform>`.
    pub dir: PathBuf,
    /// The files the manifest lists (relative `/` paths), plus the manifest itself.
    pub files: Vec<String>,
    /// Total bytes of `files`.
    pub bytes: u64,
    /// BLAKE3 of the depot's `dist-manifest.json`.
    pub manifest_blake3: String,
    /// The commit the depot's binary was built from (its manifest's `git`, ADR-5002).
    pub git: String,
}

/// What [`check_package`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageCheck {
    /// The engine version of the package.
    pub version: String,
    /// The git revision its binaries were built from; `windows <rev>, linux <rev>` when the two
    /// depots were built from different commits of one engine version (ADR-5002).
    pub git: String,
    /// The checked depots, Windows first.
    pub depots: Vec<DepotSource>,
    /// Every problem; empty means the package can be staged.
    pub problems: Vec<Problem>,
}

/// True when a path names Direct Order 27 content (`do27`, `direct_order`, `directorder`, any case).
#[must_use]
pub fn names_do27(path: &str) -> bool {
    let lower = path.to_ascii_lowercase().replace(['-', ' '], "_");
    lower.contains("do27") || lower.contains("direct_order") || lower.contains("directorder")
}

/// Walks `dir` without following links: regular files as relative `/` paths, and a problem for
/// every link or special file.
fn walk_regular(dir: &Path, problems: &mut Vec<Problem>) -> Result<Vec<String>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>, problems: &mut Vec<Problem>) -> Result<()> {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("reading {}", dir.display()))?;
        entries.sort();
        for path in entries {
            let meta = fs::symlink_metadata(&path).with_context(|| format!("reading {}", path.display()))?;
            let rel = relative(root, &path)?;
            if meta.file_type().is_symlink() || !(meta.is_dir() || meta.is_file()) {
                problems.push(Problem::new(
                    "steam_package_special_file",
                    format!("{rel}: a link or special file; depots take regular files only"),
                ));
            } else if meta.is_dir() {
                walk(root, &path, out, problems)?;
            } else {
                out.push(rel);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out, problems)?;
    Ok(out)
}

fn relative(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside {}", path.display(), root.display()))?
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/"))
}

/// Checks a finished demo package: its summary (`demo-package.json`: kind, shipping profile,
/// `panic = "abort"`, both targets), and for each depot content folder no link or special file,
/// no Direct Order 27 path, a shipping `dist-manifest.json` for the right target, and every file's
/// size and BLAKE3 against it (`dist --verify`). Both depots must be one engine version, and each
/// depot's build id must be known and equal the one `demo-package.json` records for it (a package
/// without per-target ids needs one id for both depots; ADR-5002).
///
/// # Errors
/// Only when a folder that exists cannot be read; package defects are [`PackageCheck::problems`].
pub fn check_package(dir: &Path) -> Result<PackageCheck> {
    let mut check = PackageCheck::default();
    if !dir.is_dir() {
        check.problems.push(Problem::new(
            "steam_package_missing",
            format!(
                "{} is not a folder: build the package first (`cargo xtask demo-package`)",
                dir.display()
            ),
        ));
        return Ok(check);
    }
    let summary_path = dir.join("demo-package.json");
    let summary: Option<Value> = fs::read(&summary_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let Some(summary) = summary else {
        check.problems.push(Problem::new(
            "steam_package_invalid",
            format!("{} is missing or not JSON", summary_path.display()),
        ));
        return Ok(check);
    };
    for (key, want) in [
        ("kind", PACKAGE_KIND),
        ("profile", "shipping"),
        ("panic", "abort"),
    ] {
        if summary[key].as_str() != Some(want) {
            check.problems.push(Problem::new(
                "steam_package_invalid",
                format!("demo-package.json {key} is {}, not {want:?}", summary[key]),
            ));
        }
    }
    let named: Vec<String> = summary["targets"]
        .as_array()
        .map(|t| {
            t.iter()
                .map(|e| e["target"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    for name in &named {
        if !Platform::ALL.iter().any(|p| p.name() == name) {
            check.problems.push(Problem::new(
                "steam_package_invalid",
                format!("demo-package.json names target {name:?}; the Steam depots are windows and linux"),
            ));
        }
    }
    if named.iter().collect::<BTreeSet<_>>().len() != named.len() {
        check.problems.push(Problem::new(
            "steam_package_invalid",
            "demo-package.json names a target twice",
        ));
    }
    let recorded = |name: &str| -> Option<String> {
        summary["targets"].as_array().and_then(|rows| {
            rows.iter()
                .find(|row| row["target"].as_str() == Some(name))
                .and_then(|row| row["git"].as_str())
                .map(str::to_string)
        })
    };
    let per_target_ids = Platform::ALL.iter().any(|p| recorded(p.name()).is_some());
    let mut versions = BTreeSet::new();
    let mut gits = Vec::new();
    for platform in Platform::ALL {
        if !named.iter().any(|n| n == platform.name()) {
            check.problems.push(Problem::new(
                "steam_package_target_missing",
                format!(
                    "the package has no {} build; rebuild with `cargo xtask demo-package --targets windows,linux`",
                    platform.name()
                ),
            ));
            continue;
        }
        let content = dir.join("depot").join("content").join(platform.name());
        if !content.is_dir() {
            check.problems.push(Problem::new(
                "steam_package_target_missing",
                format!("{} is missing", content.display()),
            ));
            continue;
        }
        let found = walk_regular(&content, &mut check.problems)?;
        for rel in found.iter().filter(|r| names_do27(r)) {
            check.problems.push(Problem::new(
                "steam_package_do27_content",
                format!(
                    "{}/{rel}: Direct Order 27 content never ships in the demo",
                    platform.name()
                ),
            ));
        }
        let manifest_path = content.join(dist::MANIFEST_FILE);
        let Ok(manifest_bytes) = fs::read(&manifest_path) else {
            check.problems.push(Problem::new(
                "steam_package_invalid",
                format!("{} is missing", manifest_path.display()),
            ));
            continue;
        };
        let Ok(manifest) = serde_json::from_slice::<Value>(&manifest_bytes) else {
            check.problems.push(Problem::new(
                "steam_package_invalid",
                format!("{} is not JSON", manifest_path.display()),
            ));
            continue;
        };
        if manifest["profile"].as_str() != Some("shipping") {
            check.problems.push(Problem::new(
                "steam_package_not_shipping",
                format!(
                    "{}: built with profile {}, the demo ships the `shipping` profile",
                    platform.name(),
                    manifest["profile"]
                ),
            ));
        }
        if manifest["target"].as_str() != Some(platform.dist_target().name()) {
            check.problems.push(Problem::new(
                "steam_package_invalid",
                format!(
                    "{}: dist-manifest.json is for target {}, not {}",
                    platform.name(),
                    manifest["target"],
                    platform.dist_target().name()
                ),
            ));
        }
        // Links were reported above; only run the hash check on a folder without them, since
        // `dist --verify` follows links.
        if !check
            .problems
            .iter()
            .any(|p| p.code == "steam_package_special_file")
        {
            for problem in dist::verify(&content)? {
                check.problems.push(Problem::new(
                    "steam_package_hash_mismatch",
                    format!("{}: {problem}", platform.name()),
                ));
            }
        }
        let version = manifest["engine_version"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let git = manifest["git"].as_str().unwrap_or_default().to_string();
        if git.is_empty() || git == "unknown" {
            check.problems.push(Problem::new(
                "steam_package_git_unknown",
                format!(
                    "{}: the bundle records no build id; rebuild the package with --git-{} <rev>",
                    platform.name(),
                    platform.name()
                ),
            ));
        }
        if let Some(want) = recorded(platform.name())
            && want != git
        {
            check.problems.push(Problem::new(
                "steam_package_mismatch",
                format!(
                    "{}: the depot was built from {git}, demo-package.json records {want}",
                    platform.name()
                ),
            ));
        }
        versions.insert(version.clone());
        gits.push((platform, git.clone()));
        check.version = version;
        let mut files = vec![dist::MANIFEST_FILE.to_string()];
        let mut bytes = manifest_bytes.len() as u64;
        for file in manifest["files"].as_array().into_iter().flatten() {
            if let Some(path) = file["path"].as_str() {
                files.push(path.to_string());
                bytes += file["bytes"].as_u64().unwrap_or(0);
            }
        }
        check.depots.push(DepotSource {
            platform,
            dir: content,
            files,
            bytes,
            manifest_blake3: blake3::hash(&manifest_bytes).to_hex().to_string(),
            git,
        });
    }
    let distinct: BTreeSet<&str> = gits.iter().map(|(_, g)| g.as_str()).collect();
    if versions.len() > 1 || (!per_target_ids && distinct.len() > 1) {
        check.problems.push(Problem::new(
            "steam_package_mismatch",
            format!("the depots come from different builds (engine versions {versions:?}, git {distinct:?})"),
        ));
    }
    check.git = if distinct.len() > 1 {
        gits.iter()
            .map(|(p, g)| format!("{} {g}", p.name()))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        distinct.into_iter().next().unwrap_or_default().to_string()
    };
    Ok(check)
}

/// Escapes a VDF string value (backslash and double quote).
#[must_use]
pub fn vdf_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Fills `{{KEY}}` placeholders of a template from `vars`.
///
/// # Errors
/// `steam_template_placeholder` when a placeholder has no value (the template and the code
/// disagree).
pub fn render(template: &str, vars: &[(&str, String)]) -> Result<String> {
    let mut text = template.to_string();
    for (key, value) in vars {
        text = text.replace(&format!("{{{{{key}}}}}"), value);
    }
    if let Some(at) = text.find("{{") {
        let rest = &text[at..];
        let end = rest.find("}}").map_or(rest.len(), |e| e + 2);
        bail!("steam_template_placeholder: {} has no value", &rest[..end]);
    }
    Ok(text)
}

/// The variables every VDF template takes (values VDF-escaped).
fn vdf_vars(ids: &SteamIds, version: &str, git: &str, preview: bool) -> Vec<(&'static str, String)> {
    vec![
        ("APP_ID", ids.app.to_string()),
        ("DEPOT_ID_WINDOWS", ids.depot_windows.to_string()),
        ("DEPOT_ID_LINUX", ids.depot_linux.to_string()),
        ("DEMO_NAME", vdf_escape(demo_name())),
        ("VERSION", vdf_escape(version)),
        ("GIT", vdf_escape(git)),
        ("PREVIEW", if preview { "1" } else { "0" }.to_string()),
    ]
}

/// The app build script: `Preview` 1 for steamcmd's preview (nothing uploaded), 0 to upload.
///
/// # Errors
/// A template placeholder without a value.
pub fn app_build_vdf(ids: &SteamIds, version: &str, git: &str, preview: bool) -> Result<String> {
    render(APP_BUILD_TEMPLATE, &vdf_vars(ids, version, git, preview))
}

/// The depot build script of `platform`.
///
/// # Errors
/// A template placeholder without a value.
pub fn depot_build_vdf(ids: &SteamIds, platform: Platform) -> Result<String> {
    let template = match platform {
        Platform::Windows => DEPOT_WINDOWS_TEMPLATE,
        Platform::Linux => DEPOT_LINUX_TEMPLATE,
    };
    render(template, &vdf_vars(ids, "", "", false))
}

/// Bytes as the whole decimal megabytes a store page's storage line gives (rounded up, at least 1).
#[must_use]
pub fn install_mb(bytes: u64) -> u64 {
    bytes.div_ceil(1_000_000).max(1)
}

/// The store page texts rendered with the demo's name and each depot's install size (the Windows
/// and the Linux depot differ, so each requirement block names its own): (file name, text).
///
/// # Errors
/// A template placeholder without a value.
pub fn store_texts(windows_bytes: u64, linux_bytes: u64) -> Result<Vec<(&'static str, String)>> {
    let vars = [
        ("DEMO_NAME", demo_name().to_string()),
        ("INSTALL_MB_WINDOWS", install_mb(windows_bytes).to_string()),
        ("INSTALL_MB_LINUX", install_mb(linux_bytes).to_string()),
    ];
    let mut assets = format!(
        "{} store graphics (sizes in pixels; render them with the shot list in\n\
         steamworks/STEAM-DEMO-KIT.md)\n\n",
        demo_name()
    );
    for a in STORE_ASSETS {
        assets.push_str(&format!(
            "{:<24} {:>4} x {:<4} {:<9} {}\n",
            a.name,
            a.width,
            a.height,
            if a.required { "required" } else { "optional" },
            a.file
        ));
    }
    Ok(vec![
        (
            "short-description.txt",
            render(SHORT_DESCRIPTION_TEMPLATE, &vars)?,
        ),
        ("about.bbcode", render(ABOUT_TEMPLATE, &vars)?),
        (
            "system-requirements.txt",
            render(SYSTEM_REQUIREMENTS_TEMPLATE, &vars)?,
        ),
        ("graphics.txt", assets),
    ])
}

/// Quotes a path for a POSIX shell when it needs it.
#[must_use]
pub fn shell_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+:=@,".contains(&b))
    {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The steamcmd command that runs `script`; the account is [`LOGIN_PLACEHOLDER`].
#[must_use]
pub fn steamcmd_command(script: &Path) -> String {
    format!(
        "steamcmd +login {LOGIN_PLACEHOLDER} +run_app_build {} +quit",
        shell_quote(&script.display().to_string())
    )
}

/// The result of a staging run.
#[derive(Debug, Clone, PartialEq)]
pub struct Staged {
    /// The staging folder.
    pub out: PathBuf,
    /// The steamcmd preview command (nothing uploaded).
    pub preview_command: String,
    /// The steamcmd upload command.
    pub upload_command: String,
    /// The plan written to [`PLAN_FILE`].
    pub plan: Value,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming {}", tmp.display()))
}

/// Refuses an output folder that is not empty and is not an earlier staging folder, or that
/// overlaps the package.
///
/// # Errors
/// `steam_stage_out_not_empty`, `steam_stage_out_overlaps`.
pub fn check_out(out: &Path, package: &Path) -> Result<()> {
    let norm = |p: &Path| -> PathBuf {
        p.components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect()
    };
    let (o, p) = (norm(out), norm(package));
    if o.starts_with(&p) || p.starts_with(&o) {
        bail!(
            "steam_stage_out_overlaps: {} and the package {} overlap; stage somewhere else",
            out.display(),
            package.display()
        );
    }
    if out.exists() {
        let ours = out.join(PLAN_FILE).is_file() || out.join(STAGING_MARKER).is_file();
        let empty = fs::read_dir(out)
            .with_context(|| format!("reading {}", out.display()))?
            .next()
            .is_none();
        if !ours && !empty {
            bail!(
                "steam_stage_out_not_empty: {} holds other files and is not a staging folder; \
                 move it away or pass another --out",
                out.display()
            );
        }
    }
    Ok(())
}

/// Stages a checked package into `out`: copies each depot's listed files, re-verifies the copies
/// against their manifests, writes the scripts, the store texts, the commands and the plan.
///
/// # Errors
/// [`check_out`] refusals, a copy failing, or `steam_stage_hash_mismatch` when a copy does not
/// match its manifest.
pub fn stage(check: &PackageCheck, package: &Path, ids: &SteamIds, out: &Path) -> Result<Staged> {
    if !check.problems.is_empty() {
        bail!(
            "the package has {} problem(s); nothing staged",
            check.problems.len()
        );
    }
    check_out(out, package)?;
    if out.exists() {
        fs::remove_dir_all(out).with_context(|| format!("clearing {}", out.display()))?;
    }
    write_atomic(
        &out.join(STAGING_MARKER),
        b"steam-demo-upload is writing this folder\n",
    )?;
    let mut depots = Vec::new();
    let mut install_bytes = 0u64;
    let mut platform_bytes = [0u64; 2];
    for depot in &check.depots {
        let dest = out.join("content").join(depot.platform.name());
        for rel in &depot.files {
            let to = dest.join(rel);
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            }
            // `fs::copy` keeps the permission bits, so the Linux binary stays executable.
            fs::copy(depot.dir.join(rel), &to).with_context(|| format!("copying {rel}"))?;
        }
        let problems = dist::verify(&dest)?;
        if !problems.is_empty() {
            bail!(
                "steam_stage_hash_mismatch: the staged {} depot differs from its manifest:\n  {}",
                depot.platform.name(),
                problems.join("\n  ")
            );
        }
        install_bytes = install_bytes.max(depot.bytes);
        platform_bytes[usize::from(depot.platform == Platform::Linux)] = depot.bytes;
        let id = depot.platform.depot_id(ids);
        let script = format!("depot_build_{id}.vdf");
        write_atomic(
            &out.join("scripts").join(&script),
            depot_build_vdf(ids, depot.platform)?.as_bytes(),
        )?;
        depots.push(json!({
            "platform": depot.platform.name(),
            "depot_id": id,
            "script": format!("scripts/{script}"),
            "content": format!("content/{}", depot.platform.name()),
            "files": depot.files.len(),
            "bytes": depot.bytes,
            "manifest_blake3": depot.manifest_blake3,
            "git": depot.git,
        }));
    }
    let scripts = out.join("scripts");
    let upload_script = scripts.join(format!("app_build_{}.vdf", ids.app));
    let preview_script = scripts.join(format!("app_build_{}_preview.vdf", ids.app));
    write_atomic(
        &upload_script,
        app_build_vdf(ids, &check.version, &check.git, false)?.as_bytes(),
    )?;
    write_atomic(
        &preview_script,
        app_build_vdf(ids, &check.version, &check.git, true)?.as_bytes(),
    )?;
    fs::create_dir_all(out.join("output")).with_context(|| format!("creating {}/output", out.display()))?;
    for (file, text) in store_texts(platform_bytes[0], platform_bytes[1])? {
        write_atomic(&out.join("store").join(file), text.as_bytes())?;
    }
    let preview_command = steamcmd_command(&preview_script);
    let upload_command = steamcmd_command(&upload_script);
    let notes = format!(
        "{name} {version} ({git}): Steam upload, the owner's step\n\n\
         Nothing here holds or asks for a password. Type your build account in place of\n\
         {LOGIN_PLACEHOLDER}; steamcmd asks for the password and the Steam Guard code itself.\n\n\
         1. Preview (checks the scripts and content, uploads nothing):\n   {preview_command}\n\n\
         2. Upload:\n   {upload_command}\n\n\
         3. SetLive is empty: the build appears under Builds on the partner site and goes live only\n   \
         when you set it live on a branch there.\n",
        name = demo_name(),
        version = check.version,
        git = check.git,
    );
    write_atomic(&out.join("UPLOAD-COMMAND.txt"), notes.as_bytes())?;
    let plan = json!({
        "kind": PLAN_KIND,
        "schema": 1,
        "demo_name": demo_name(),
        "engine_version": check.version,
        "git": check.git,
        "app_id": ids.app,
        "depots": depots,
        "install_bytes": install_bytes,
        "install_mb": {
            "windows": install_mb(platform_bytes[0]),
            "linux": install_mb(platform_bytes[1]),
        },
        "commands": { "preview": preview_command, "upload": upload_command },
        "steamcmd_run": false,
        "credentials_stored": false,
    });
    write_atomic(
        &out.join(PLAN_FILE),
        format!("{}\n", serde_json::to_string_pretty(&plan)?).as_bytes(),
    )?;
    fs::remove_file(out.join(STAGING_MARKER)).with_context(|| format!("removing {STAGING_MARKER}"))?;
    Ok(Staged {
        out: out.to_path_buf(),
        preview_command,
        upload_command,
        plan,
    })
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("the current directory")?
            .join(path))
    }
}

fn report_problems(problems: &[Problem], json_out: bool) -> ExitCode {
    if json_out {
        let list: Vec<Value> = problems
            .iter()
            .map(|p| json!({"code": p.code, "message": p.message}))
            .collect();
        println!("{}", json!({"ok": false, "problems": list}));
    }
    for p in problems {
        eprintln!("error: {}: {}", p.code, p.message);
    }
    eprintln!("steam-demo-upload: {} problem(s); nothing staged", problems.len());
    ExitCode::FAILURE
}

/// Runs `cargo xtask steam-demo-upload`. Exit 0 staged, 1 any problem (nothing staged).
///
/// # Errors
/// A folder that cannot be read or written.
pub fn run(args: &Args, root: &Path) -> Result<ExitCode> {
    if !args.dry_run {
        return Ok(report_problems(
            &[Problem::new(
                "steam_upload_dry_run_required",
                "pass --dry-run: this command stages the upload and prints the steamcmd command; \
                 it never runs steamcmd or logs in",
            )],
            args.json,
        ));
    }
    let ids = match parse_ids(&args.app, &args.depot_win, &args.depot_linux) {
        Ok(ids) => ids,
        Err(problems) => return Ok(report_problems(&problems, args.json)),
    };
    let package = absolute(
        args.package
            .clone()
            .unwrap_or_else(|| root.join("target").join("demo-package")),
    )?;
    let out = absolute(
        args.out
            .clone()
            .unwrap_or_else(|| root.join("target").join("steam-demo-upload")),
    )?;
    let check = check_package(&package)?;
    if !check.problems.is_empty() {
        return Ok(report_problems(&check.problems, args.json));
    }
    let staged = match stage(&check, &package, &ids, &out) {
        Ok(staged) => staged,
        Err(error) => {
            let text = format!("{error:#}");
            let code = [
                "steam_stage_out_not_empty",
                "steam_stage_out_overlaps",
                "steam_stage_hash_mismatch",
            ]
            .into_iter()
            .find(|c| text.starts_with(c));
            let Some(code) = code else { return Err(error) };
            let message = text.trim_start_matches(code).trim_start_matches(": ").to_string();
            return Ok(report_problems(&[Problem::new(code, message)], args.json));
        }
    };
    if args.json {
        println!(
            "{}",
            json!({"ok": true, "out": staged.out.display().to_string(), "plan": staged.plan})
        );
    } else {
        println!(
            "steam-demo-upload: staged {} {} ({}) for app {} in {}",
            demo_name(),
            check.version,
            check.git,
            ids.app,
            staged.out.display()
        );
        for depot in &check.depots {
            println!(
                "  depot {} {}: {} files, {} bytes, hashes match",
                depot.platform.depot_id(&ids),
                depot.platform.name(),
                depot.files.len(),
                depot.bytes
            );
        }
        println!("dry run: steamcmd was not run. Your commands (preview first):");
        println!("  {}", staged.preview_command);
        println!("  {}", staged.upload_command);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: SteamIds = SteamIds {
        app: 4_100_000,
        depot_windows: 4_100_001,
        depot_linux: 4_100_002,
    };

    // Header-only PE/ELF images for package integrity tests, NOT runnable binaries (the same
    // public header layout as the demo-package fixtures, ADR-868).
    fn fixture_binaries(dir: &Path) {
        let mut pe = vec![0_u8; 512];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x40_u32.to_le_bytes());
        pe[0x40..0x44].copy_from_slice(b"PE\0\0");
        pe[0x44..0x46].copy_from_slice(&0x8664_u16.to_le_bytes());
        pe[0x54..0x56].copy_from_slice(&240_u16.to_le_bytes());
        pe[0x58..0x5a].copy_from_slice(&0x20b_u16.to_le_bytes());
        pe[0x9c..0x9e].copy_from_slice(&3_u16.to_le_bytes());
        write_atomic(&dir.join("showcase.exe"), &pe).expect("PE fixture");
        let mut elf = vec![0_u8; 128];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[0x12..0x14].copy_from_slice(&62_u16.to_le_bytes());
        elf[0x28..0x30].copy_from_slice(&64_u64.to_le_bytes());
        elf[0x3a..0x3c].copy_from_slice(&64_u16.to_le_bytes());
        elf[0x3c..0x3e].copy_from_slice(&1_u16.to_le_bytes());
        write_atomic(&dir.join("showcase"), &elf).expect("ELF fixture");
    }

    /// A package laid out as `cargo xtask demo-package` writes it, built with `dist` from
    /// header-only binaries. Returns (fixture base, package dir).
    fn package_fixture(tag: &str, profile: &str) -> (PathBuf, PathBuf) {
        let base = regia_test_utils::unique_temp_dir(tag);
        let root = base.join("repo");
        for name in ["LICENSE", "THIRD_PARTY_NOTICES.md"] {
            write_atomic(&root.join(name), b"Test fixture license\n").expect("license");
        }
        fixture_binaries(&base.join("bin"));
        let package = base.join("package");
        for platform in Platform::ALL {
            let bundles = base.join("bundles").join(platform.name());
            let args = dist::Args {
                target: Some(platform.dist_target()),
                bins: vec!["showcase".into()],
                profile: profile.into(),
                out: Some(bundles.clone()),
                from: Some(base.join("bin")),
                no_archive: true,
                git: Some("abc1234".into()),
                ..dist::Args::default()
            };
            assert_eq!(dist::run(&args, &root).expect("dist fixture"), ExitCode::SUCCESS);
            let bundle = bundles.join(format!(
                "regia-{}-{}",
                platform.dist_target().name(),
                dist::engine_version()
            ));
            let content = package.join("depot/content").join(platform.name());
            fs::create_dir_all(content.parent().expect("parent")).expect("depot dir");
            fs::rename(&bundle, &content).expect("move bundle");
        }
        let summary = json!({
            "kind": PACKAGE_KIND,
            "profile": "shipping",
            "panic": "abort",
            "targets": [{"target": "windows"}, {"target": "linux"}],
        });
        write_atomic(
            &package.join("demo-package.json"),
            serde_json::to_string_pretty(&summary).expect("json").as_bytes(),
        )
        .expect("summary");
        (base, package)
    }

    fn codes(check: &PackageCheck) -> Vec<&'static str> {
        check.problems.iter().map(|p| p.code).collect()
    }

    #[test]
    fn the_demo_name_is_one_plain_line() {
        let name = demo_name();
        assert!(!name.is_empty());
        assert_eq!(DEMO_NAME_FILE.trim_end().lines().count(), 1);
        assert!(!name.contains(['"', '\\', '{', '}']), "{name}");
    }

    #[test]
    fn render_fills_every_placeholder_and_refuses_one_without_a_value() {
        let text = render("a {{X}} b {{X}} {{Y}}", &[("X", "1".into()), ("Y", "2".into())]).expect("render");
        assert_eq!(text, "a 1 b 1 2");
        let err = render("a {{MISSING}} b", &[("X", "1".into())]).expect_err("unknown placeholder");
        assert!(
            err.to_string()
                .contains("steam_template_placeholder: {{MISSING}}"),
            "{err}"
        );
    }

    #[test]
    fn the_vdf_templates_name_both_depots_and_never_go_live() {
        for preview in [false, true] {
            let app = app_build_vdf(&IDS, "0.5.0", "abc1234", preview).expect("app build");
            assert!(app.contains("\"AppID\" \"4100000\""), "{app}");
            assert!(
                app.contains("\"SetLive\" \"\""),
                "nothing goes live by itself: {app}"
            );
            assert!(
                app.contains(&format!("\"Preview\" \"{}\"", u8::from(preview))),
                "{app}"
            );
            assert!(app.contains("\"4100001\" \"depot_build_4100001.vdf\""), "{app}");
            assert!(app.contains("\"4100002\" \"depot_build_4100002.vdf\""), "{app}");
            assert!(
                app.contains(&format!("\"Desc\" \"{} 0.5.0 (abc1234)\"", demo_name())),
                "{app}"
            );
            assert!(app.contains("\"ContentRoot\" \"../content/\""));
            assert!(app.contains("\"BuildOutput\" \"../output/\""));
        }
        let win = depot_build_vdf(&IDS, Platform::Windows).expect("win");
        assert!(win.contains("\"DepotID\" \"4100001\"") && win.contains("\"LocalPath\" \"windows/*\""));
        assert!(
            win.contains("\"FileExclusion\" \"*.pdb\""),
            "no debug symbols in the depot"
        );
        let linux = depot_build_vdf(&IDS, Platform::Linux).expect("linux");
        assert!(linux.contains("\"DepotID\" \"4100002\"") && linux.contains("\"LocalPath\" \"linux/*\""));
        assert!(linux.contains("\"recursive\" \"1\""));
        // A git text with a quote cannot break out of its VDF string.
        let app = app_build_vdf(&IDS, "0.5.0", "a\"b", false).expect("escaped");
        assert!(app.contains("(a\\\"b)"), "{app}");
    }

    #[test]
    fn ids_must_be_positive_decimal_and_distinct() {
        assert_eq!(parse_ids("4100000", "4100001", "4100002"), Ok(IDS));
        for (app, win, linux) in [
            ("0", "1", "2"),
            ("abc", "1", "2"),
            ("1", "-2", "3"),
            ("1", "2", "+3"),
            ("", "2", "3"),
        ] {
            let problems = parse_ids(app, win, linux).expect_err("invalid id");
            assert!(
                problems.iter().all(|p| p.code == "steam_id_invalid"),
                "{problems:?}"
            );
        }
        let problems = parse_ids("5", "5", "6").expect_err("duplicate");
        assert_eq!(problems[0].code, "steam_id_duplicate");
    }

    #[test]
    fn without_dry_run_nothing_is_staged() {
        let base = regia_test_utils::unique_temp_dir("steam_upload_no_dry_run");
        let out = base.join("out");
        let args = Args {
            app: "4100000".into(),
            depot_win: "4100001".into(),
            depot_linux: "4100002".into(),
            package: Some(base.join("package")),
            out: Some(out.clone()),
            dry_run: false,
            json: false,
        };
        assert_eq!(run(&args, &base).expect("run"), ExitCode::FAILURE);
        assert!(!out.exists());
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn a_complete_package_stages_into_the_steamcmd_layout_with_matching_hashes() {
        let (base, package) = package_fixture("steam_upload_stage", "shipping");
        let check = check_package(&package).expect("check");
        assert!(check.problems.is_empty(), "{:?}", check.problems);
        assert_eq!(check.depots.len(), 2);
        let out = base.join("upload");
        let staged = stage(&check, &package, &IDS, &out).expect("stage");
        for depot in &check.depots {
            let dest = out.join("content").join(depot.platform.name());
            assert!(dist::verify(&dest).expect("verify").is_empty());
            for rel in &depot.files {
                assert_eq!(
                    fs::read(depot.dir.join(rel)).expect("source"),
                    fs::read(dest.join(rel)).expect("copy"),
                    "{rel}"
                );
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(out.join("content/linux/showcase"))
                .expect("bin")
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0, "the Linux binary stays executable");
        }
        for file in [
            "scripts/app_build_4100000.vdf",
            "scripts/app_build_4100000_preview.vdf",
            "scripts/depot_build_4100001.vdf",
            "scripts/depot_build_4100002.vdf",
            "store/short-description.txt",
            "store/about.bbcode",
            "store/system-requirements.txt",
            "store/graphics.txt",
            "UPLOAD-COMMAND.txt",
            PLAN_FILE,
        ] {
            assert!(out.join(file).is_file(), "{file}");
        }
        assert!(out.join("output").is_dir());
        assert!(!out.join(STAGING_MARKER).exists());
        let plan: Value =
            serde_json::from_slice(&fs::read(out.join(PLAN_FILE)).expect("plan")).expect("json");
        assert_eq!(plan["kind"], PLAN_KIND);
        assert_eq!(plan["demo_name"], demo_name());
        assert_eq!(plan["steamcmd_run"], false);
        assert_eq!(plan["credentials_stored"], false);
        assert_eq!(plan["depots"][1]["depot_id"], 4_100_002);
        let script = out.join("scripts/app_build_4100000.vdf");
        assert_eq!(
            staged.upload_command,
            format!(
                "steamcmd +login {LOGIN_PLACEHOLDER} +run_app_build {} +quit",
                shell_quote(&script.display().to_string())
            )
        );
        assert!(staged.preview_command.contains("app_build_4100000_preview.vdf"));
        let notes = fs::read_to_string(out.join("UPLOAD-COMMAND.txt")).expect("notes");
        assert!(notes.contains(&staged.preview_command) && notes.contains(&staged.upload_command));
        let short = fs::read_to_string(out.join("store/short-description.txt")).expect("short");
        assert!(!short.contains("{{"));
        let requirements = fs::read_to_string(out.join("store/system-requirements.txt")).expect("req");
        assert!(
            requirements.contains("Storage: 1 MB available space"),
            "{requirements}"
        );
        // A second run replaces an earlier staging folder.
        stage(&check, &package, &IDS, &out).expect("restage");
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn a_changed_or_unlisted_depot_file_stops_before_staging() {
        let (base, package) = package_fixture("steam_upload_tamper", "shipping");
        let readme = package.join("depot/content/windows/README.txt");
        let mut bytes = fs::read(&readme).expect("readme");
        bytes.push(b'!');
        write_atomic(&readme, &bytes).expect("tamper");
        write_atomic(&package.join("depot/content/linux/extra.bin"), b"unlisted").expect("extra");
        let check = check_package(&package).expect("check");
        let mismatches: Vec<_> = check
            .problems
            .iter()
            .filter(|p| p.code == "steam_package_hash_mismatch")
            .collect();
        assert!(
            mismatches
                .iter()
                .any(|p| p.message.starts_with("windows: README.txt")),
            "{mismatches:?}"
        );
        assert!(
            mismatches
                .iter()
                .any(|p| p.message.starts_with("linux: extra.bin")),
            "{mismatches:?}"
        );
        let out = base.join("upload");
        assert!(stage(&check, &package, &IDS, &out).is_err());
        assert!(!out.exists(), "nothing staged");
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn a_package_without_both_shipping_targets_is_refused() {
        let (base, package) = package_fixture("steam_upload_targets", "release");
        let check = check_package(&package).expect("check");
        assert!(
            codes(&check).contains(&"steam_package_not_shipping"),
            "{:?}",
            check.problems
        );
        fs::remove_dir_all(package.join("depot/content/linux")).expect("drop linux");
        let summary_path = package.join("demo-package.json");
        let mut summary: Value =
            serde_json::from_slice(&fs::read(&summary_path).expect("read")).expect("json");
        summary["targets"] = json!([{"target": "windows"}, {"target": "windows"}]);
        summary["panic"] = json!("unwind");
        write_atomic(&summary_path, summary.to_string().as_bytes()).expect("write");
        let check = check_package(&package).expect("check");
        let found = codes(&check);
        assert!(found.contains(&"steam_package_target_missing"), "{found:?}");
        assert!(found.contains(&"steam_package_invalid"), "{found:?}");
        assert!(
            check.problems.iter().any(|p| p.message.contains("panic")),
            "{:?}",
            check.problems
        );
        let missing = check_package(&base.join("nowhere")).expect("check");
        assert_eq!(codes(&missing), ["steam_package_missing"]);
        fs::remove_file(&summary_path).expect("drop summary");
        assert_eq!(
            codes(&check_package(&package).expect("check")),
            ["steam_package_invalid"]
        );
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn direct_order_27_paths_and_mixed_builds_are_refused() {
        assert!(
            names_do27("content/DO27_map.ron")
                && names_do27("Direct-Order/x")
                && names_do27("directorder.pak")
        );
        assert!(!names_do27("content/scene/forest.ron"));
        let (base, package) = package_fixture("steam_upload_do27", "shipping");
        write_atomic(&package.join("depot/content/windows/content/do27_map.ron"), b"x").expect("do27");
        let manifest_path = package.join("depot/content/linux").join(dist::MANIFEST_FILE);
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read")).expect("json");
        manifest["git"] = json!("another-build");
        write_atomic(&manifest_path, manifest.to_string().as_bytes()).expect("write");
        let found = codes(&check_package(&package).expect("check"));
        assert!(found.contains(&"steam_package_do27_content"), "{found:?}");
        assert!(found.contains(&"steam_package_mismatch"), "{found:?}");
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn links_in_a_depot_are_refused_without_following_them() {
        let (base, package) = package_fixture("steam_upload_link", "shipping");
        std::os::unix::fs::symlink("/etc/hostname", package.join("depot/content/linux/link.txt"))
            .expect("link");
        let check = check_package(&package).expect("check");
        assert!(
            codes(&check).contains(&"steam_package_special_file"),
            "{:?}",
            check.problems
        );
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn a_foreign_or_overlapping_out_folder_is_left_alone() {
        let (base, package) = package_fixture("steam_upload_out", "shipping");
        let check = check_package(&package).expect("check");
        let foreign = base.join("mine");
        write_atomic(&foreign.join("keep.txt"), b"precious").expect("foreign");
        let err = stage(&check, &package, &IDS, &foreign).expect_err("foreign folder");
        assert!(err.to_string().starts_with("steam_stage_out_not_empty"), "{err}");
        assert_eq!(fs::read(foreign.join("keep.txt")).expect("kept"), b"precious");
        let err = stage(&check, &package, &IDS, &package.join("upload")).expect_err("inside the package");
        assert!(err.to_string().starts_with("steam_stage_out_overlaps"), "{err}");
        let empty = base.join("empty");
        fs::create_dir_all(&empty).expect("empty");
        stage(&check, &package, &IDS, &empty).expect("an empty folder is fine");
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn the_store_texts_fit_the_page_and_make_no_comparisons() {
        let texts = store_texts(123_456_789, 277_000_001).expect("texts");
        let short = &texts
            .iter()
            .find(|(f, _)| *f == "short-description.txt")
            .expect("short")
            .1;
        assert!(
            short.trim().chars().count() <= SHORT_DESCRIPTION_MAX,
            "{} chars",
            short.trim().chars().count()
        );
        let about = &texts.iter().find(|(f, _)| *f == "about.bbcode").expect("about").1;
        assert!(about.contains(demo_name()));
        let requirements = &texts
            .iter()
            .find(|(f, _)| *f == "system-requirements.txt")
            .expect("req")
            .1;
        // Each platform's block names its own depot (audit row 9).
        let (windows, linux) = requirements
            .split_once("STEAMOS + LINUX")
            .expect("a Linux block after the Windows blocks");
        assert!(
            windows.contains("Storage: 124 MB available space") && !windows.contains("Storage: 278 MB"),
            "{requirements}"
        );
        assert!(
            linux.contains("Storage: 278 MB available space") && !linux.contains("124 MB"),
            "{requirements}"
        );
        assert_eq!(
            (install_mb(0), install_mb(1_000_000), install_mb(1_000_001)),
            (1, 1, 2)
        );
        // The page lists only what this build shows (audit row 3): no night, no rain or snow, no
        // overcast sky (the panel stops at cloudy, ADR-5419), no Steam Deck until DECK-CHECK.md
        // passes on this package.
        for (file, text) in &texts {
            let lower = text.to_ascii_lowercase();
            let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).collect();
            for claim in ["night", "rain", "snow", "storm", "overcast", "dusk"] {
                assert!(!words.contains(&claim), "{file}: {claim:?}");
            }
            for claim in ["steam deck", "full controller"] {
                assert!(!lower.contains(claim), "{file}: {claim:?}");
            }
        }
        for (file, text) in &texts {
            let lower = text.to_ascii_lowercase();
            let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).collect();
            for banned in [
                "first",
                "unique",
                "best",
                "only",
                "unreal",
                "unity",
                "godot",
                "cryengine",
                "bevy",
                "fyrox",
            ] {
                assert!(!words.contains(&banned), "{file}: {banned:?}");
            }
            for phrase in ["better than", "faster than", "world's", "never before"] {
                assert!(!lower.contains(phrase), "{file}: {phrase:?}");
            }
        }
    }

    #[test]
    fn the_kit_doc_lists_every_store_graphic_size() {
        let doc = include_str!("../../steamworks/STEAM-DEMO-KIT.md");
        for asset in STORE_ASSETS {
            let row = format!("| {} | {} x {} |", asset.name, asset.width, asset.height);
            assert!(
                doc.contains(&row),
                "steamworks/STEAM-DEMO-KIT.md lacks the row {row:?}"
            );
            assert!(doc.contains(asset.file), "{}", asset.file);
        }
    }

    #[test]
    fn each_depot_carries_its_recorded_build_id() {
        // ADR-5002: the v0.2.20 inputs were built from two commits of one engine version; the
        // package records each, and a depot that disagrees with the record is refused.
        let (base, package) = package_fixture("steam_upload_git", "shipping");
        let summary_path = package.join("demo-package.json");
        let mut summary: Value =
            serde_json::from_slice(&fs::read(&summary_path).expect("read")).expect("json");
        summary["targets"] = json!([
            {"target": "windows", "git": "abc1234"},
            {"target": "linux", "git": "def5678"},
        ]);
        write_atomic(&summary_path, summary.to_string().as_bytes()).expect("summary");
        let manifest_path = package.join("depot/content/linux").join(dist::MANIFEST_FILE);
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read")).expect("json");
        manifest["git"] = json!("def5678");
        write_atomic(&manifest_path, manifest.to_string().as_bytes()).expect("write");
        let check = check_package(&package).expect("check");
        assert!(check.problems.is_empty(), "{:?}", check.problems);
        assert_eq!(check.git, "windows abc1234, linux def5678");
        let app = app_build_vdf(&IDS, &check.version, &check.git, false).expect("vdf");
        assert!(app.contains("(windows abc1234, linux def5678)"), "{app}");
        let staged = stage(&check, &package, &IDS, &base.join("upload")).expect("stage");
        assert_eq!(staged.plan["depots"][1]["git"], "def5678");
        // A depot that is not the recorded build.
        summary["targets"][1]["git"] = json!("0000000");
        write_atomic(&summary_path, summary.to_string().as_bytes()).expect("summary");
        let found = codes(&check_package(&package).expect("check"));
        assert_eq!(found, ["steam_package_mismatch"], "{found:?}");
        // A depot without a build id.
        manifest["git"] = json!("unknown");
        write_atomic(&manifest_path, manifest.to_string().as_bytes()).expect("write");
        summary["targets"][1]["git"] = json!("unknown");
        write_atomic(&summary_path, summary.to_string().as_bytes()).expect("summary");
        let found = codes(&check_package(&package).expect("check"));
        assert!(found.contains(&"steam_package_git_unknown"), "{found:?}");
        fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn shell_quote_leaves_plain_paths_and_quotes_the_rest() {
        assert_eq!(shell_quote("/srv/a-b/c_d.vdf"), "/srv/a-b/c_d.vdf");
        assert_eq!(shell_quote("/My Files/x.vdf"), "'/My Files/x.vdf'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn this_command_never_starts_a_process() {
        // The module stages and prints; steamcmd is the owner's step (ADR-4899).
        let source = include_str!("steam_demo_upload.rs");
        let needle = ["Command", "::new"].concat();
        assert!(
            !source.contains(&needle),
            "steam_demo_upload must not spawn processes"
        );
        assert!(!source.contains(&["std::process::", "Command"].concat()));
    }
}
