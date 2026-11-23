// SPDX-License-Identifier: MPL-2.0
//! `regia steam` (specs/24 § 3): the Steam depot/VDF layout `regia package --steam` produces,
//! and `regia steam upload`, which runs `steamcmd +run_app_build` with credentials from the
//! environment only — Regia Engine never writes them anywhere.
//!
//! The VDF shapes are Valve's documented steamcmd format, as publicly exercised by the open-source
//! game-ci/steam-deploy action (MIT): an `"appbuild"` document with `appid`, `desc`,
//! `buildoutput`, `contentroot`, `setlive` and a `depots` map, and a `"DepotBuildConfig"`
//! document with `DepotID` and a `FileMapping` (`LocalPath`, `DepotPath`, `recursive`).
//! Valve numbers a standard app's first depot `app_id + 1`; projects may deviate via
//! `[steam] depots` in `Regia.toml`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::manifest::Manifest;
use crate::process;
use regia_steam::{
    STEAM_APPID_FILE, action_manifest_file_name, action_manifest_from_ron, autocloud_document,
};

/// The Steam build account, read from the environment. Credentials never appear in `Regia.toml`,
/// in VDFs, in logs or on disk (specs/24 § 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamCredentials {
    /// Build account name (`STEAM_USERNAME`).
    pub username: String,
    /// Build account password (`STEAM_PASSWORD`). Never logged.
    pub password: String,
    /// Steam Guard / TOTP code (`STEAM_TOTP`), when the build account requires one.
    pub totp: Option<String>,
}

impl SteamCredentials {
    /// Reads the steamcmd login from the environment, with a clear error naming what is missing.
    pub fn from_env() -> Result<Self> {
        let username = regia_core::brand::env_var("STEAM_USERNAME")
            .context("set STEAM_USERNAME to the Steam build account (specs/24 § 3)")?;
        let password = regia_core::brand::env_var("STEAM_PASSWORD")
            .context("set STEAM_PASSWORD to the Steam build account's password")?;
        let totp = regia_core::brand::env_var("STEAM_TOTP")
            .ok()
            .filter(|code| !code.is_empty());
        Ok(Self {
            username,
            password,
            totp,
        })
    }
}

/// `regia steam <command>`.
#[derive(Debug, Subcommand)]
pub enum SteamCommand {
    /// Upload the packaged Steam depots to a branch with `steamcmd` (specs/24 § 3).
    Upload(UploadArgs),
    /// Write the Steam Input in-game actions file (`game_actions_<appid>.vdf`) generated from
    /// `config/input.ron` (ADR-350, ADR-526).
    InputManifest(InputManifestArgs),
    /// Print the controls screen of each input context for a controller family and check that
    /// every action is playable on a controller alone (ADR-1332).
    Layout(LayoutArgs),
}

/// `regia steam layout`: the controller layout of the project's `config/input.ron`.
#[derive(Debug, Clone, Args)]
pub struct LayoutArgs {
    /// Project directory (contains `config/input.ron`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Glyph family the sheet is labeled for: xbox, playstation, switch or deck.
    #[arg(long, default_value = "deck")]
    pub family: String,
    /// Only this input context (default: every context, highest priority first).
    #[arg(long)]
    pub context: Option<String>,
    /// Asset folder to check the four glyph sets in, relative to the project (for example
    /// `assets`); the check is skipped without it.
    #[arg(long)]
    pub glyphs: Option<PathBuf>,
    /// An action allowed to have no gamepad binding (a debug console key). Repeat for more.
    #[arg(long = "allow-keyboard-only")]
    pub allow_keyboard_only: Vec<String>,
    /// Fail (exit code 1) when the check finds an error, for CI and `ship_check`-style gates.
    #[arg(long, default_value_t = false)]
    pub check: bool,
}

/// The `regia steam layout` report for `args`: the sheets as text, then one line per issue, and
/// whether any issue is an error.
pub fn layout_report(args: &LayoutArgs) -> Result<(String, bool)> {
    use regia_steam::deck_glyphs::family_from_dir;
    use regia_steam::deck_layout::{
        LayoutCheck, check_layout, has_errors, input_config_from_ron, layout_sheet, layout_sheets,
    };
    let Some(family) = family_from_dir(&args.family) else {
        bail!(
            "unknown glyph family `{}`: use xbox, playstation, switch or deck",
            args.family
        );
    };
    let path = args.project.join(INPUT_CONFIG_FILE);
    let ron = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let config = input_config_from_ron(&ron)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("controller layout of {}", path.display()))?;
    let sheets = match &args.context {
        Some(context) => match layout_sheet(&config, context, family) {
            Some(sheet) => vec![sheet],
            None => bail!("{} has no input context `{context}`", path.display()),
        },
        None => layout_sheets(&config, family),
    };
    let check = LayoutCheck {
        keyboard_only_ok: args.allow_keyboard_only.clone(),
        glyph_root: args.glyphs.as_ref().map(|dir| args.project.join(dir)),
    };
    let issues = check_layout(&config, &check);
    let mut text = String::new();
    for sheet in &sheets {
        text.push_str(&sheet.to_text());
    }
    if issues.is_empty() {
        text.push_str("no issues\n");
    }
    for issue in &issues {
        text.push_str(&format!(
            "{} {}: {}\n",
            issue.severity.name(),
            issue.code,
            issue.message
        ));
    }
    Ok((text, has_errors(&issues)))
}

/// Runs `regia steam layout`: prints the report; with `--check`, an error in it fails the run.
pub fn layout(args: &LayoutArgs) -> Result<()> {
    let (text, errors) = layout_report(args)?;
    print!("{text}");
    if args.check && errors {
        bail!("the controller layout check found errors (above)");
    }
    Ok(())
}

/// `regia steam input-manifest`: generates Steam Input's in-game actions file from the
/// project's input config, one action set per input context.
#[derive(Debug, Clone, Args)]
pub struct InputManifestArgs {
    /// Project directory (contains `Regia.toml` and `config/input.ron`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Output file (default `<project>/dist/steam/game_actions_<appid>.vdf`; needs
    /// `[steam] app_id` in Regia.toml).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Print the manifest on stdout instead of writing a file.
    #[arg(long, default_value_t = false)]
    pub stdout: bool,
}

/// The project's input config file, relative to the project directory.
pub const INPUT_CONFIG_FILE: &str = "config/input.ron";

/// The Steam Input actions file text for the project in `project_dir`.
pub fn input_manifest_text(project_dir: &Path) -> Result<String> {
    let path = project_dir.join(INPUT_CONFIG_FILE);
    let ron = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    action_manifest_from_ron(&ron).with_context(|| format!("Steam Input manifest from {}", path.display()))
}

/// Runs `regia steam input-manifest`: writes the file (atomically: a temporary file renamed
/// into place) and returns its path, or prints the text with `--stdout` and returns `None`.
pub fn input_manifest(args: &InputManifestArgs) -> Result<Option<PathBuf>> {
    let text = input_manifest_text(&args.project)?;
    if args.stdout {
        print!("{text}");
        return Ok(None);
    }
    let out = match &args.out {
        Some(out) => out.clone(),
        None => {
            let manifest = Manifest::load(&args.project)?;
            if !manifest.steam.is_configured() {
                bail!(
                    "`steam input-manifest` names the file after `[steam] app_id` in Regia.toml; \
                     add the app id or pass --out"
                );
            }
            steam_dir(&args.project.join("dist")).join(action_manifest_file_name(manifest.steam.app_id))
        }
    };
    write_vdf_atomically(&out, &text)?;
    Ok(Some(out))
}

/// Writes `text` to `out` through a sibling `.tmp` file and a rename, creating the directory.
fn write_vdf_atomically(out: &Path, text: &str) -> Result<()> {
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = out.with_extension("vdf.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, out).with_context(|| format!("writing {}", out.display()))?;
    Ok(())
}

/// The `regia package --steam` step for Steam Input: when the project has `config/input.ron`,
/// writes `game_actions_<appid>.vdf` into `steam_dir` next to the build VDFs (upload it on the
/// partner site). A config Steam Input cannot express (a 3D action, a name collision, bad RON) is
/// a warning, not a packaging failure: the game still runs on the gamepad path. Actions a
/// controller cannot reach ([`package_layout_errors`]) are warned about too. Returns the written
/// file.
pub fn package_input_manifest(
    project_dir: &Path,
    manifest: &Manifest,
    steam_dir: &Path,
) -> Result<Option<PathBuf>> {
    if !project_dir.join(INPUT_CONFIG_FILE).is_file() {
        return Ok(None);
    }
    for message in package_layout_errors(project_dir) {
        tracing::warn!(
            target: "regia_cli",
            "controller layout: {message} (`regia steam layout --check` lists every issue)"
        );
    }
    match input_manifest_text(project_dir) {
        Ok(text) => {
            let out = steam_dir.join(action_manifest_file_name(manifest.steam.app_id));
            write_vdf_atomically(&out, &text)?;
            Ok(Some(out))
        }
        Err(err) => {
            tracing::warn!(target: "regia_cli", "no Steam Input actions file in the package: {err:#}");
            Ok(None)
        }
    }
}

/// The controller-layout errors of the project's `config/input.ron` (ADR-1332): one message per
/// action a controller cannot reach. Warnings and an unreadable config give nothing here (the
/// manifest step reports a bad config).
pub fn package_layout_errors(project_dir: &Path) -> Vec<String> {
    use regia_steam::deck_layout::{LayoutCheck, LayoutSeverity, check_layout, input_config_from_ron};
    let Ok(ron) = std::fs::read_to_string(project_dir.join(INPUT_CONFIG_FILE)) else {
        return Vec::new();
    };
    let Ok(config) = input_config_from_ron(&ron) else {
        return Vec::new();
    };
    check_layout(&config, &LayoutCheck::default())
        .into_iter()
        .filter(|issue| issue.severity == LayoutSeverity::Error)
        .map(|issue| issue.message)
        .collect()
}

/// `regia steam upload`: runs `steamcmd +run_app_build` on the VDFs regenerated from
/// `[steam]`, pointed at the depots staged by earlier `regia package --steam` runs.
#[derive(Debug, Clone, Args)]
pub struct UploadArgs {
    /// Branch to publish the build on. Must be listed in `[steam] branches`, or `default`
    /// (which additionally requires `--live` and a confirmation).
    #[arg(long)]
    pub branch: String,
    /// Allow publishing to the `default` branch — the one players get.
    #[arg(long, default_value_t = false)]
    pub live: bool,
    /// Skip the `default`-branch confirmation prompt (non-interactive CI). No effect on other
    /// branches.
    #[arg(long, default_value_t = false)]
    pub yes: bool,
    /// Regenerate the VDFs and print the steamcmd invocation without uploading.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Project directory (contains `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Distribution root with the packaged depots (default `<project>/dist`).
    #[arg(long)]
    pub dist: Option<PathBuf>,
}

/// `<out>/steam`: where a `--steam` package run writes the VDFs and `steamcloud.md`.
pub fn steam_dir(out_dir: &Path) -> PathBuf {
    out_dir.join("steam")
}

/// Escapes a value for a VDF string literal (quotes and backslashes).
fn vdf_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The `depot_build_<depotid>.vdf` document: map the staged package into the depot root.
fn depot_build_vdf(depot_id: u32, local_path: &str) -> String {
    format!(
        "\"DepotBuildConfig\"\n{{\n  \"DepotID\" \"{depot_id}\"\n  \"FileMapping\"\n  {{\n    \
         \"LocalPath\" \"./{local_path}/*\"\n    \"DepotPath\" \".\"\n    \"recursive\" \"1\"\n  }}\n  \
         \"FileExclusion\" \"*.pdb\"\n}}\n"
    )
}

/// The `app_build_<appid>.vdf` document: what depots this build contains and where it goes live.
/// `depots` is (depot id, depot vdf file name) pairs, `setlive` the branch to publish on (empty
/// publishes nowhere — the build waits on the partner site).
fn app_build_vdf(
    manifest: &Manifest,
    contentroot: &Path,
    buildoutput: &Path,
    setlive: &str,
    depots: &[(u32, String)],
) -> String {
    let mut vdf = String::new();
    vdf.push_str("\"appbuild\"\n{\n");
    vdf.push_str(&format!("  \"appid\" \"{}\"\n", manifest.steam.app_id));
    vdf.push_str(&format!(
        "  \"desc\" \"{}\"\n",
        vdf_escape(&format!(
            "{} {} build {}",
            manifest.project.name, manifest.project.version, manifest.project.build_number
        ))
    ));
    vdf.push_str(&format!(
        "  \"buildoutput\" \"{}\"\n",
        vdf_escape(&regia_asset_pipeline::generators::plain_path(buildoutput))
    ));
    vdf.push_str(&format!(
        "  \"contentroot\" \"{}\"\n",
        vdf_escape(&regia_asset_pipeline::generators::plain_path(contentroot))
    ));
    vdf.push_str(&format!("  \"setlive\" \"{}\"\n", vdf_escape(setlive)));
    vdf.push_str("  \"depots\"\n  {\n");
    for (depot_id, vdf_name) in depots {
        vdf.push_str(&format!("    \"{depot_id}\" \"{}\"\n", vdf_escape(vdf_name)));
    }
    vdf.push_str("  }\n}\n");
    vdf
}

/// Rejects a manifest value that is joined into a depot path unless it is one plain path segment:
/// no separators, no `.`/`..`, no control characters. Otherwise a name such as `../..` would make
/// the depot (and the VDF `LocalPath`) point outside `dist`.
fn ensure_single_segment(what: &str, value: &str) -> Result<()> {
    let bad = value.is_empty()
        || value == "."
        || value == ".."
        || value.contains(['/', '\\', ':'])
        || value.chars().any(char::is_control);
    if bad {
        bail!(
            "{what} `{value}` must be a single path segment (no `/`, `\\`, `:`, `..` or control characters) to be used as a Steam depot path"
        );
    }
    Ok(())
}

/// One packaged depot the upload found: the platform's staging directory and its depot id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackagedDepot {
    /// Regia Engine platform name (`linux`, `windows`).
    pub platform: String,
    /// Steam depot id from `[steam] depots`.
    pub depot_id: u32,
    /// The staged package directory (`<dist>/<platform>/<app id slug>`).
    pub staging: PathBuf,
}

/// The depot staging directories `regia package --steam` produced under `dist`, one per
/// platform that has both a `[steam] depots` entry and a non-empty staging directory.
pub fn packaged_depots(manifest: &Manifest, dist: &Path) -> Result<Vec<PackagedDepot>> {
    let name = crate::commands::package::staging_slug(manifest);
    ensure_single_segment("staging directory (the slugged [project] id or name)", &name)?;
    for platform in manifest.steam.depots.keys() {
        ensure_single_segment("[steam.depots] platform", platform)?;
    }
    let mut found = Vec::new();
    for (platform, depot_id) in &manifest.steam.depots {
        let staging = dist.join(platform).join(&name);
        if staging.is_dir() {
            found.push(PackagedDepot {
                platform: platform.clone(),
                depot_id: *depot_id,
                staging,
            });
        }
    }
    Ok(found)
}

/// Completes a staged desktop package for Steam (specs/24 § 3): `steam_appid.txt` for dev runs,
/// the Steamworks SDK redistributable next to the binary (when `STEAM_SDK_LOCATION` is set), the
/// Auto-Cloud configuration doc, and `app_build_<appid>.vdf` + `depot_build_<depotid>.vdf` under
/// `<out>/steam`. Returns the steam dir.
pub fn finish_package(
    manifest: &Manifest,
    platform: &str,
    staging: &Path,
    binary_name: &str,
    out_dir: &Path,
) -> Result<PathBuf> {
    let steam = &manifest.steam;
    if !steam.is_configured() {
        bail!(
            "--steam needs `[steam] app_id` in Regia.toml (specs/24 § 3); add the app id from \
             the Steamworks partner site"
        );
    }
    let Some(depot_id) = steam.depot_of(platform) else {
        bail!("--steam needs `[steam.depots] {platform} = <depot id>` in Regia.toml");
    };

    // Dev runs outside Steam learn the app id from this file next to the binary; the Steam client
    // ignores it (specs/24 § 2 "Init").
    std::fs::write(staging.join(STEAM_APPID_FILE), format!("{}\n", steam.app_id))
        .context("writing steam_appid.txt")?;

    // The SDK redistributable must ship next to the game binary. It is never committed to the
    // project (Valve Steamworks SDK Access Agreement); the developer points STEAM_SDK_LOCATION
    // at the unpacked SDK (crates/regia_steam/README.md).
    let redistributable = match platform {
        "linux" => ("redistributable_bin/linux64/libsteam_api.so", "libsteam_api.so"),
        "windows" => ("redistributable_bin/win64/steam_api64.dll", "steam_api64.dll"),
        other => bail!("--steam is a desktop option; no depot for `{other}`"),
    };
    match regia_core::brand::env_var_os("STEAM_SDK_LOCATION") {
        Some(sdk) => {
            let source = Path::new(&sdk).join(redistributable.0);
            std::fs::copy(&source, staging.join(redistributable.1)).with_context(|| {
                format!(
                    "copying {} — is STEAM_SDK_LOCATION the unpacked Steamworks SDK?",
                    source.display()
                )
            })?;
            tracing::info!(target: "regia_cli", "Steamworks redistributable: {}", redistributable.1);
        }
        None => {
            tracing::warn!(
                target: "regia_cli",
                "STEAM_SDK_LOCATION is not set: the package has no {} — Steam builds need it \
                 next to the binary. Download the Steamworks SDK from \
                 partner.steamgames.com and set STEAM_SDK_LOCATION (see \
                 crates/regia_steam/README.md)",
                redistributable.1
            );
        }
    }
    if platform == "linux" {
        // Shipping Linux builds link with an `$ORIGIN` rpath, so the binary finds
        // libsteam_api.so next to itself, and against the runtime's glibc (`--glibc`, checked by
        // `ship_check`), so it starts in Steam Linux Runtime 3.0 and on the Deck.
        tracing::info!(
            target: "regia_cli",
            "Steam Linux depot: test it inside Steam Linux Runtime 3.0 (sniper) before upload \
             (book/user-guide/packaging.md: \"Linux shipping builds and the Steam runtime\")"
        );
    }

    // The Auto-Cloud configuration for `user://` (specs/24 § 2 "Cloud saves"): what to enter on
    // the partner site so saves sync without game code.
    let steam_dir = steam_dir(out_dir);
    std::fs::create_dir_all(&steam_dir)?;
    std::fs::write(steam_dir.join("steamcloud.md"), autocloud_document(binary_name))
        .context("writing steamcloud.md")?;

    // steamcmd resolves relative roots against its scripts, not the caller's cwd. The staging
    // and steam directories exist now, so canonicalize their parents before deriving buildoutput.
    // Keep the distribution root above the caller's platform directory, even when that
    // platform directory is a symlink. The parent of a bare relative `linux` is empty.
    let content_root = match out_dir.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => out_dir,
    }
    .canonicalize()
    .context("resolving Steam distribution directory")?;
    let absolute_steam_dir = steam_dir
        .canonicalize()
        .context("resolving Steam scripts directory")?;
    // The depot layout: the staged package is the depot content; the VDFs describe it.
    let depot_vdf_name = format!("depot_build_{depot_id}.vdf");
    std::fs::write(
        steam_dir.join(&depot_vdf_name),
        depot_build_vdf(
            depot_id,
            &vdf_escape(&format!(
                "{platform}/{}",
                crate::commands::package::staging_slug(manifest)
            )),
        ),
    )
    .with_context(|| format!("writing {depot_vdf_name}"))?;
    std::fs::write(
        steam_dir.join(format!("app_build_{}.vdf", steam.app_id)),
        app_build_vdf(
            manifest,
            &content_root,
            &absolute_steam_dir.join("buildoutput"),
            "", // a package run never publishes anything; `steam upload --branch` decides
            &[(depot_id, depot_vdf_name)],
        ),
    )
    .with_context(|| format!("writing app_build_{}.vdf", steam.app_id))?;
    Ok(steam_dir)
}

/// The dry-run steamcmd command line with the password and Steam Guard code redacted.
fn dry_run_line(steamcmd: &str, credentials: &SteamCredentials, app_build: &Path) -> String {
    let guard = credentials
        .totp
        .as_ref()
        .map(|_| " +set_steam_guard_code [code redacted]")
        .unwrap_or("");
    format!(
        "dry run: {steamcmd} +login {} [password redacted]{guard} +run_app_build {} +quit",
        credentials.username,
        regia_asset_pipeline::generators::plain_path(app_build)
    )
}

/// Runs `regia steam upload` (specs/24 § 3): validates the branch, regenerates the VDFs for
/// every packaged depot under `dist`, then hands them to `steamcmd +run_app_build` with the
/// credentials from the environment. A `default`-branch upload needs `--live` plus the app id
/// typed on stdin (or `--yes` in CI); anything else publishes nothing without this command.
pub fn upload(args: &UploadArgs, credentials: &SteamCredentials) -> Result<()> {
    let project_dir = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    let manifest = Manifest::load(&project_dir)?;
    let steam = &manifest.steam;
    if !steam.is_configured() {
        bail!("`steam upload` needs `[steam] app_id` in Regia.toml (specs/24 § 3)");
    }

    // Branch guard: beta branches must be configured; `default` is a second, explicit decision.
    let branch = args.branch.as_str();
    if branch == "default" {
        if !args.live {
            bail!(
                "publishing to the `default` branch needs `--live` — this is the build every \
                 player gets (specs/24 § 3)"
            );
        }
        if !args.yes {
            confirm_default_branch(steam.app_id)?;
        }
    } else if !steam.branches.iter().any(|b| b == branch) {
        bail!(
            "branch `{branch}` is not in `[steam] branches` ({}); add it on the Steamworks \
             partner site and in Regia.toml",
            steam.branches.join(", ")
        );
    }

    let dist = args.dist.clone().unwrap_or_else(|| project_dir.join("dist"));
    let depots = packaged_depots(&manifest, &dist)?;
    if depots.is_empty() {
        bail!(
            "no packaged Steam depots under {} — run `regia package --platform <p> --steam` \
             first (specs/24 § 3)",
            dist.display()
        );
    }

    let dist = dist
        .canonicalize()
        .context("resolving Steam distribution directory")?;

    // Regenerate the VDFs for the combined build into `dist/steam-upload`.
    let upload_dir = dist.join("steam-upload");
    std::fs::create_dir_all(&upload_dir)?;
    let mut depot_vdfs = Vec::new();
    for depot in &depots {
        let vdf_name = format!("depot_build_{}.vdf", depot.depot_id);
        std::fs::write(
            upload_dir.join(&vdf_name),
            depot_build_vdf(
                depot.depot_id,
                &vdf_escape(&format!(
                    "{}/{}",
                    depot.platform,
                    crate::commands::package::staging_slug(&manifest)
                )),
            ),
        )
        .with_context(|| format!("writing {vdf_name}"))?;
        depot_vdfs.push((depot.depot_id, vdf_name));
    }
    let app_build = upload_dir.join(format!("app_build_{}.vdf", steam.app_id));
    std::fs::write(
        &app_build,
        app_build_vdf(
            &manifest,
            &dist,
            &upload_dir.join("buildoutput"),
            branch,
            &depot_vdfs,
        ),
    )
    .with_context(|| format!("writing {}", app_build.display()))?;

    // steamcmd: REGIA_STEAMCMD overrides the binary (like REGIA_GRADLE for Gradle).
    let steamcmd = regia_core::brand::env_var_os("REGIA_STEAMCMD").unwrap_or_else(|| "steamcmd".into());
    let mut command = Command::new(&steamcmd);
    command
        .arg("+login")
        .arg(&credentials.username)
        .arg(&credentials.password);
    if let Some(code) = &credentials.totp {
        command.arg("+set_steam_guard_code").arg(code);
    }
    command
        .arg("+run_app_build")
        .arg(regia_asset_pipeline::generators::plain_path(&app_build))
        .arg("+quit");

    if args.dry_run {
        println!(
            "{}",
            dry_run_line(&steamcmd.to_string_lossy(), credentials, &app_build)
        );
        println!(
            "setlive: branch `{branch}`, depots: {}",
            depots
                .iter()
                .map(|d| format!("{} ({})", d.depot_id, d.platform))
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(());
    }

    let mut secrets = vec![credentials.password.as_str()];
    if let Some(code) = &credentials.totp {
        secrets.push(code.as_str());
    }
    process::run_redacted(
        &mut command,
        "Install steamcmd (store.steampowered.com) or set \
         REGIA_STEAMCMD to its path",
        &secrets,
    )
    .context("steamcmd upload failed")?;
    println!("uploaded {branch} build of app {}", steam.app_id);
    Ok(())
}

/// Asks the human to type the app id before publishing to the `default` branch. Aborts on EOF
/// (non-interactive shells): CI passes `--yes` after deciding.
fn confirm_default_branch(app_id: u32) -> Result<()> {
    println!(
        "This publishes to the `default` branch — the build every player gets.\n\
         Type the app id ({app_id}) to confirm:"
    );
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading the confirmation (stdin closed; pass --yes in CI)")?;
    if line.trim() != app_id.to_string() {
        bail!("confirmation did not match the app id; aborting (nothing was uploaded)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    fn steam_manifest() -> Manifest {
        Manifest::parse(
            r#"
[project]
name = "Direct Order 27"
id = "com.piercing.do27"
version = "0.4.1"
build_number = 7

[steam]
app_id = 480
branches = ["beta", "playtest"]

[steam.depots]
linux = 480001
windows = 480002
"#,
        )
        .expect("manifest")
    }

    /// A `config/input.ron` with one gamepad-bound action in the default context.
    const PAD_INPUT_RON: &str = r#"(
    actions: { "Jump": Button },
    contexts: {
        "Gameplay": (
            priority: 0,
            consume_lower: false,
            bindings: [(action: "Jump", input: GamepadButton(South))],
        ),
    },
)"#;

    /// A project directory with a Regia.toml (Steam app 480) and an input config.
    fn input_project(input_ron: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir("cli_steam_input_manifest");
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"Pad Test\"\nid = \"com.example.padtest\"\nversion = \"0.1.0\"\n\n[steam]\napp_id = 480\n",
        )
        .expect("write Regia.toml");
        std::fs::create_dir_all(dir.join("config")).expect("config dir");
        std::fs::write(dir.join(INPUT_CONFIG_FILE), input_ron).expect("write input.ron");
        dir
    }

    #[test]
    fn input_manifest_is_written_under_dist_steam_with_the_app_id() {
        let project = input_project(PAD_INPUT_RON);
        let args = InputManifestArgs {
            project: project.clone(),
            out: None,
            stdout: false,
        };
        let out = input_manifest(&args).expect("manifest written").expect("a file");
        assert_eq!(out, project.join("dist/steam/game_actions_480.vdf"));
        let text = std::fs::read_to_string(&out).expect("read back");
        assert!(text.starts_with("\"Action Manifest\""));
        assert!(text.contains("\"GAMEPLAY\""));
        assert!(text.contains("\"gameplay_jump\""));
        assert!(
            !out.with_extension("vdf.tmp").exists(),
            "the temporary file was renamed"
        );
        // An explicit --out wins and needs no app id.
        let custom = project.join("custom/actions.vdf");
        let args = InputManifestArgs {
            out: Some(custom.clone()),
            ..args
        };
        assert_eq!(input_manifest(&args).expect("custom out"), Some(custom.clone()));
        assert_eq!(std::fs::read_to_string(custom).expect("read custom"), text);
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn package_step_writes_the_actions_file_and_only_warns_on_a_bad_config() {
        let project = input_project(PAD_INPUT_RON);
        let manifest = Manifest::load(&project).expect("manifest");
        let steam = project.join("dist/linux/steam");
        let out = package_input_manifest(&project, &manifest, &steam)
            .expect("package step")
            .expect("written");
        assert_eq!(out, steam.join("game_actions_480.vdf"));
        assert!(
            std::fs::read_to_string(&out)
                .expect("read")
                .contains("\"gameplay_jump\"")
        );

        std::fs::write(project.join(INPUT_CONFIG_FILE), "(actions: {").expect("break config");
        std::fs::remove_file(&out).expect("remove");
        assert_eq!(
            package_input_manifest(&project, &manifest, &steam).expect("only a warning"),
            None
        );
        assert!(!out.exists());
        std::fs::remove_file(project.join(INPUT_CONFIG_FILE)).expect("remove config");
        assert_eq!(
            package_input_manifest(&project, &manifest, &steam).expect("no config"),
            None
        );
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn input_manifest_reports_a_broken_or_missing_config() {
        let project = input_project("(actions: {");
        let args = InputManifestArgs {
            project: project.clone(),
            out: None,
            stdout: false,
        };
        let err = input_manifest(&args).expect_err("bad RON");
        assert!(format!("{err:#}").contains("input config RON"), "{err:#}");
        std::fs::remove_file(project.join(INPUT_CONFIG_FILE)).expect("remove config");
        let err = input_manifest(&args).expect_err("no config");
        assert!(format!("{err:#}").contains("config/input.ron"), "{err:#}");
        let _ = std::fs::remove_dir_all(project);
    }

    /// Gameplay with a pad-less console key and two actions sharing East; a menu context.
    const LAYOUT_INPUT_RON: &str = r#"(
    actions: { "Jump": Button, "Crouch": Button, "Slide": Button, "Console": Button, "Accept": Button },
    contexts: {
        "Gameplay": (
            priority: 0,
            consume_lower: false,
            bindings: [
                (action: "Jump", input: Key("Space")),
                (action: "Jump", input: GamepadButton(South)),
                (action: "Crouch", input: GamepadButton(East)),
                (action: "Slide", input: GamepadButton(East)),
                (action: "Console", input: Key("Backquote")),
            ],
        ),
        "Menu": (
            priority: 10,
            consume_lower: true,
            bindings: [(action: "Accept", input: GamepadButton(South))],
        ),
    },
)"#;

    fn layout_args(project: &Path) -> LayoutArgs {
        LayoutArgs {
            project: project.to_path_buf(),
            family: "deck".into(),
            context: None,
            glyphs: None,
            allow_keyboard_only: Vec::new(),
            check: true,
        }
    }

    #[test]
    fn layout_prints_the_sheets_and_fails_the_check_on_an_unplayable_action() {
        let project = input_project(LAYOUT_INPUT_RON);
        let args = layout_args(&project);
        let (text, errors) = layout_report(&args).expect("report");
        assert!(errors);
        assert!(
            text.starts_with("Menu (deck)\n  A  Accept\nGameplay (deck)\n"),
            "{text}"
        );
        assert!(text.contains("  B  Crouch, Slide\n"), "{text}");
        assert!(text.contains("  keyboard or mouse only: Console\n"), "{text}");
        assert!(
            text.contains("error no_gamepad_binding: `Console` in `Gameplay`"),
            "{text}"
        );
        assert!(
            text.contains("warning control_conflict: Crouch, Slide share `east`"),
            "{text}"
        );
        let err = layout(&args).expect_err("--check fails on the error");
        assert!(format!("{err:#}").contains("controller layout check"), "{err:#}");

        // Allowing the console key leaves only the warning: the check passes.
        let args = LayoutArgs {
            allow_keyboard_only: vec!["Console".into()],
            family: "playstation".into(),
            context: Some("Gameplay".into()),
            ..layout_args(&project)
        };
        let (text, errors) = layout_report(&args).expect("report");
        assert!(!errors, "{text}");
        assert!(
            text.starts_with("Gameplay (playstation)\n  Cross   Jump\n  Circle  Crouch, Slide\n"),
            "{text}"
        );
        layout(&args).expect("only warnings");
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn the_package_step_names_unreachable_actions() {
        let project = input_project(LAYOUT_INPUT_RON);
        let errors = package_layout_errors(&project);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("`Console` in `Gameplay`"), "{errors:?}");
        let manifest = Manifest::load(&project).expect("manifest");
        let steam = project.join("dist/linux/steam");
        assert!(
            package_input_manifest(&project, &manifest, &steam)
                .expect("a layout error is a warning")
                .is_some()
        );
        std::fs::write(project.join(INPUT_CONFIG_FILE), PAD_INPUT_RON).expect("pad-only config");
        assert!(package_layout_errors(&project).is_empty());
        std::fs::write(project.join(INPUT_CONFIG_FILE), "(actions: {").expect("break config");
        assert!(
            package_layout_errors(&project).is_empty(),
            "bad RON is the manifest step's to report"
        );
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn layout_checks_glyphs_and_rejects_bad_arguments() {
        let project = input_project(PAD_INPUT_RON);
        let args = LayoutArgs {
            glyphs: Some(PathBuf::from("assets")),
            ..layout_args(&project)
        };
        let (text, errors) = layout_report(&args).expect("report");
        assert!(!errors, "glyph gaps are warnings: {text}");
        assert_eq!(
            text.matches("warning missing_glyphs: 20 of 20").count(),
            4,
            "{text}"
        );

        let bad_family = LayoutArgs {
            family: "gamecube".into(),
            ..layout_args(&project)
        };
        assert!(format!("{:#}", layout_report(&bad_family).expect_err("family")).contains("gamecube"));
        let bad_context = LayoutArgs {
            context: Some("Driving".into()),
            ..layout_args(&project)
        };
        assert!(format!("{:#}", layout_report(&bad_context).expect_err("context")).contains("Driving"));
        std::fs::write(project.join(INPUT_CONFIG_FILE), "(actions: {").expect("break config");
        assert!(
            format!("{:#}", layout_report(&layout_args(&project)).expect_err("RON"))
                .contains("input config RON")
        );
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn depot_path_segments_must_be_plain() {
        assert!(ensure_single_segment("n", "Direct Order 27").is_ok());
        for bad in ["", ".", "..", "../..", "a/b", "a\\b", "C:x", "a\nb"] {
            assert!(ensure_single_segment("n", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn packaged_depots_rejects_traversal_names() {
        let mut manifest = steam_manifest();
        manifest.project.name = "../..".to_string();
        // Depots are staged under the slugged id (audit fix); `..` survives slugging unchanged.
        manifest.project.id = "..".to_string();
        let dist = regia_test_utils::unique_temp_dir("regia_cli_steam_name");
        assert!(packaged_depots(&manifest, &dist).is_err());
        // A platform key that walks out of `dist` is rejected too.
        let mut manifest = steam_manifest();
        manifest.steam.depots.insert("../x".to_string(), 480009);
        assert!(packaged_depots(&manifest, &dist).is_err());
        let _ = std::fs::remove_dir_all(&dist);
    }

    #[test]
    fn depot_build_vdf_maps_the_staged_package() {
        let vdf = depot_build_vdf(480001, "linux/Direct Order 27");
        assert!(vdf.contains("\"DepotID\" \"480001\""));
        assert!(vdf.contains("\"LocalPath\" \"./linux/Direct Order 27/*\""));
        assert!(vdf.contains("\"DepotPath\" \".\""));
        assert!(vdf.contains("\"recursive\" \"1\""));
        assert!(vdf.contains("\"FileExclusion\" \"*.pdb\""));
    }

    #[test]
    fn app_build_vdf_lists_depots_and_setlive() {
        let manifest = steam_manifest();
        let vdf = app_build_vdf(
            &manifest,
            Path::new("/games/do27/dist"),
            Path::new("/games/do27/dist/steam-upload/buildoutput"),
            "beta",
            &[(480001, "depot_build_480001.vdf".into())],
        );
        assert!(vdf.contains("\"appid\" \"480\""));
        assert!(vdf.contains("\"desc\" \"Direct Order 27 0.4.1 build 7\""));
        assert!(vdf.contains("\"setlive\" \"beta\""));
        assert!(vdf.contains("\"contentroot\" \"/games/do27/dist\""));
        assert!(vdf.contains("\"480001\" \"depot_build_480001.vdf\""));
    }

    #[test]
    fn vdf_values_are_escaped() {
        assert_eq!(vdf_escape("a\"b\\c"), "a\\\"b\\\\c");
    }

    fn staged_project(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("regia_cli_steam_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        std::fs::create_dir_all(&project).expect("mkdir project");
        std::fs::write(
            project.join("Regia.toml"),
            r#"
[project]
name = "Test Game"
id = "com.test.game"

[steam]
app_id = 480
branches = ["beta"]

[steam.depots]
linux = 480001
"#,
        )
        .expect("Regia.toml");
        // A staged linux depot: dist/linux/com.test.game with the packaged binary.
        let staging = project.join("dist").join("linux").join("com.test.game");
        std::fs::create_dir_all(staging.join("content")).expect("mkdir staging");
        std::fs::write(staging.join("Test Game"), b"elf").expect("binary");
        (project, base)
    }

    #[test]
    fn upload_finds_the_depot_the_packager_stages() -> Result<()> {
        let (project, base) = staged_project("discover");
        let manifest = Manifest::load(&project)?;
        let found = packaged_depots(&manifest, &project.join("dist"))?;
        // Name "Test Game", id "com.test.game": staged under the id slug.
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].staging.ends_with("linux/com.test.game"),
            "{:?}",
            found[0].staging
        );
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    /// A subprocess owns its cwd; other tests never see a process-global directory change.
    fn relative_vdf_paths(test: &str, package: bool) -> Result<()> {
        const CHILD: &str = "REGIA_TEST_STEAM_RELATIVE_PATHS";
        if std::env::var_os(CHILD).is_none() {
            let base = regia_test_utils::unique_temp_dir("steam-relative-paths");
            let output = Command::new(std::env::current_exe()?)
                .args(["--exact", test, "--nocapture"])
                .current_dir(&base)
                .env(CHILD, "1")
                .env_remove("STEAM_SDK_LOCATION")
                .env_remove("FERRITE_STEAM_SDK_LOCATION") // rename-keep: legacy spelling
                .output()?;
            std::fs::remove_dir_all(base)?;
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(());
        }
        std::fs::write(
            "Regia.toml",
            "[project]\nname = \"Test Game\"\nid = \"com.test.game\"\n[steam]\napp_id = 480\nbranches = [\"beta\"]\n[steam.depots]\nlinux = 480001\n",
        )?;
        let staging = Path::new("dist/linux/com.test.game");
        std::fs::create_dir_all(staging)?;
        let manifest = Manifest::load(Path::new("."))?;
        let dist = regia_asset_pipeline::generators::plain_path(&Path::new("dist").canonicalize()?);
        let (app_build, build_root) = if package {
            let steam_dir =
                finish_package(&manifest, "linux", staging, "Test Game", Path::new("dist/linux"))?;
            (steam_dir.join("app_build_480.vdf"), Path::new("dist/linux/steam"))
        } else {
            upload(
                &UploadArgs {
                    branch: "beta".into(),
                    live: false,
                    yes: false,
                    dry_run: true,
                    project: PathBuf::from("."),
                    dist: Some(PathBuf::from("dist")),
                },
                &SteamCredentials {
                    username: "test".into(),
                    password: "test".into(),
                    totp: None,
                },
            )?;
            (
                PathBuf::from("dist/steam-upload/app_build_480.vdf"),
                Path::new("dist/steam-upload"),
            )
        };
        let vdf = std::fs::read_to_string(app_build)?;
        assert!(
            vdf.contains(&format!("\"contentroot\" \"{}\"", vdf_escape(&dist))),
            "{vdf}"
        );
        let build =
            regia_asset_pipeline::generators::plain_path(&build_root.canonicalize()?.join("buildoutput"));
        assert!(
            vdf.contains(&format!("\"buildoutput\" \"{}\"", vdf_escape(&build))),
            "{vdf}"
        );
        Ok(())
    }

    #[test]
    fn package_steam_relative_paths_are_absolute_in_vdfs() -> Result<()> {
        relative_vdf_paths(
            "commands::steam::tests::package_steam_relative_paths_are_absolute_in_vdfs",
            true,
        )
    }

    #[test]
    fn steam_upload_relative_dist_uses_absolute_vdf_roots() -> Result<()> {
        relative_vdf_paths(
            "commands::steam::tests::steam_upload_relative_dist_uses_absolute_vdf_roots",
            false,
        )
    }

    #[test]
    fn steam_vdf_uses_plain_windows_drive_spelling() {
        let manifest =
            Manifest::parse("[project]\nname = \"Test\"\nid = \"com.test.game\"\n[steam]\napp_id = 480\n")
                .expect("manifest");
        let vdf = app_build_vdf(
            &manifest,
            Path::new(r"\\?\C:\dist"),
            Path::new(r"\\?\C:\out"),
            "",
            &[],
        );
        assert!(vdf.contains(r#""contentroot" "C:\\dist""#), "{vdf}");
        assert!(vdf.contains(r#""buildoutput" "C:\\out""#), "{vdf}");
        assert!(!vdf.contains('?'));
    }

    #[test]
    fn package_steam_writes_the_depot_layout_and_vdfs() -> Result<()> {
        let (project, base) = staged_project("package");
        let manifest = Manifest::load(&project)?;
        let dist = project.join("dist");
        let staging = dist.join("linux").join("com.test.game");

        let steam_dir = finish_package(&manifest, "linux", &staging, "Test Game", &dist.join("linux"))?;

        // Dev app id file next to the binary.
        assert_eq!(std::fs::read_to_string(staging.join(STEAM_APPID_FILE))?, "480\n");
        // Depot + app VDFs under <out>/steam, referencing the staged content.
        let depot_vdf = std::fs::read_to_string(steam_dir.join("depot_build_480001.vdf"))?;
        assert!(
            depot_vdf.contains("\"LocalPath\" \"./linux/com.test.game/*\""),
            "{depot_vdf}"
        );
        let app_vdf = std::fs::read_to_string(steam_dir.join("app_build_480.vdf"))?;
        assert!(app_vdf.contains("\"appid\" \"480\""));
        assert!(
            app_vdf.contains("\"setlive\" \"\""),
            "a package run publishes nothing"
        );
        assert!(app_vdf.contains("\"480001\" \"depot_build_480001.vdf\""));
        // The Auto-Cloud doc names the user-data directory of this exe.
        let cloud = std::fs::read_to_string(steam_dir.join("steamcloud.md"))?;
        assert!(cloud.contains("Test Game"), "{cloud}");
        assert!(cloud.contains("LinuxXdgDataHome"), "{cloud}");
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    #[test]
    fn package_steam_without_steam_table_fails_clearly() -> Result<()> {
        let base = std::env::temp_dir().join(format!("regia_cli_steam_{}_noid", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        std::fs::create_dir_all(&project)?;
        std::fs::write(
            project.join("Regia.toml"),
            "[project]\nname = \"T\"\nid = \"a.b\"\n",
        )?;
        let manifest = Manifest::load(&project)?;
        let err = finish_package(&manifest, "linux", &project, "T", &project).expect_err("no [steam] table");
        assert!(format!("{err:#}").contains("--steam needs `[steam] app_id`"));
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    #[test]
    fn upload_dry_run_regenerates_vdfs_and_prints_the_command() -> Result<()> {
        let (project, base) = staged_project("dryrun");
        let args = UploadArgs {
            branch: "beta".into(),
            live: false,
            yes: false,
            dry_run: true,
            project: project.clone(),
            dist: None,
        };
        // Credentials come from the environment in production; the command itself is the test's.
        upload(
            &args,
            &SteamCredentials {
                username: "builder".into(),
                password: "hunter2".into(),
                totp: None,
            },
        )?;

        let upload_dir = project.join("dist").join("steam-upload");
        let app_vdf = std::fs::read_to_string(upload_dir.join("app_build_480.vdf")).expect("app vdf");
        assert!(app_vdf.contains("\"setlive\" \"beta\""), "{app_vdf}");
        assert!(
            app_vdf.contains("\"480001\" \"depot_build_480001.vdf\""),
            "the packaged linux depot is included"
        );
        assert!(upload_dir.join("depot_build_480001.vdf").is_file());
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    #[test]
    fn dry_run_line_redacts_the_secrets() {
        let credentials = SteamCredentials {
            username: "builder".into(),
            password: "hunter2".into(),
            totp: Some("ABCDE".into()),
        };
        let line = dry_run_line("steamcmd", &credentials, Path::new("/x/app_build_480.vdf"));
        assert!(line.contains("+login builder [password redacted]"), "{line}");
        assert!(line.contains("[code redacted]"), "{line}");
        assert!(!line.contains("hunter2") && !line.contains("ABCDE"), "{line}");
        assert!(
            line.ends_with("+run_app_build /x/app_build_480.vdf +quit"),
            "{line}"
        );
    }

    #[test]
    fn upload_guards_the_default_and_unknown_branches() -> Result<()> {
        let (project, base) = staged_project("guards");
        let credentials = SteamCredentials {
            username: "builder".into(),
            password: "hunter2".into(),
            totp: None,
        };
        // `default` without --live.
        let err = upload(
            &UploadArgs {
                branch: "default".into(),
                live: false,
                yes: false,
                dry_run: true,
                project: project.clone(),
                dist: None,
            },
            &credentials,
        )
        .expect_err("default needs --live");
        assert!(format!("{err:#}").contains("--live"));
        // `default` with --live but no confirmation (--yes for the test).
        upload(
            &UploadArgs {
                branch: "default".into(),
                live: true,
                yes: true,
                dry_run: true,
                project: project.clone(),
                dist: None,
            },
            &credentials,
        )?;
        // A branch not configured.
        let err = upload(
            &UploadArgs {
                branch: "nightly".into(),
                live: false,
                yes: false,
                dry_run: true,
                project: project.clone(),
                dist: None,
            },
            &credentials,
        )
        .expect_err("unknown branch");
        assert!(format!("{err:#}").contains("not in `[steam] branches`"));
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    #[test]
    fn upload_without_packaged_depots_fails() -> Result<()> {
        let base = std::env::temp_dir().join(format!("regia_cli_steam_{}_empty", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        std::fs::create_dir_all(&project)?;
        std::fs::write(
            project.join("Regia.toml"),
            r#"
[project]
name = "T"
id = "a.b"

[steam]
app_id = 480
branches = ["beta"]

[steam.depots]
linux = 480001
"#,
        )?;
        let err = upload(
            &UploadArgs {
                branch: "beta".into(),
                live: false,
                yes: false,
                dry_run: true,
                project: project.clone(),
                dist: None,
            },
            &SteamCredentials {
                username: "b".into(),
                password: "p".into(),
                totp: None,
            },
        )
        .expect_err("nothing packaged");
        assert!(format!("{err:#}").contains("no packaged Steam depots"));
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    /// Every file under `dir`, recursively (symlinks are not followed).
    fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => files_under(&path, out),
                Ok(t) if t.is_file() => out.push(path),
                _ => {}
            }
        }
    }

    /// specs/24 § 3: the steamcmd credentials come from the environment and are never written to
    /// disk. The upload writes its VDFs before the dry-run branch returns, so after a package and
    /// an upload of every branch kind no file in the project may hold the account name, the
    /// password or the Steam Guard code.
    #[test]
    fn package_and_upload_never_write_the_credentials_to_disk() -> Result<()> {
        let (project, base) = staged_project("nocreds");
        let manifest = Manifest::load(&project)?;
        let dist = project.join("dist");
        let staging = dist.join("linux").join("com.test.game");
        finish_package(&manifest, "linux", &staging, "Test Game", &dist.join("linux"))?;
        let credentials = SteamCredentials {
            username: "zq_build_account_7731".into(),
            password: "zq_secret_password_7731".into(),
            totp: Some("ZQ7731".into()),
        };
        for (branch, live) in [("beta", false), ("default", true)] {
            upload(
                &UploadArgs {
                    branch: branch.into(),
                    live,
                    yes: true,
                    dry_run: true,
                    project: project.clone(),
                    dist: None,
                },
                &credentials,
            )?;
        }
        let mut files = Vec::new();
        files_under(&base, &mut files);
        assert!(
            files.iter().any(|f| f.ends_with("app_build_480.vdf")),
            "the upload wrote its VDFs: {files:?}"
        );
        for file in &files {
            let bytes = std::fs::read(file)?;
            let text = String::from_utf8_lossy(&bytes);
            for secret in ["zq_build_account_7731", "zq_secret_password_7731", "ZQ7731"] {
                assert!(
                    !text.contains(secret),
                    "{} holds a credential ({secret})",
                    file.display()
                );
            }
        }
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    #[test]
    fn steam_dir_is_under_the_out_dir() {
        assert_eq!(
            steam_dir(Path::new("/p/dist/linux")),
            PathBuf::from("/p/dist/linux/steam")
        );
    }
}
