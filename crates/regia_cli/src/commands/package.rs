// SPDX-License-Identifier: MPL-2.0
//! `regia package`: build a distributable for one platform
//! (`docs/specs/21-tooling-and-packaging.md` § 4).
//!
//! M16 completes packaging for all active platforms:
//! - Linux: tar.gz with binary, paks, .desktop, icon
//! - Windows: zip with exe + paks
//! - Android: APK + AAB with release signing, icons, manifest from project settings

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};

use crate::android::gradle_project::{
    self, ASSET_PACKS_DIR, AssetPackModule, GENERATED_SOURCE_DIR, JNI_LIBS_DIR, Ndk, Toolchain,
};
use crate::android::{NATIVE_LIB_NAME, toolchain};
use crate::commands::build_profile::{self, ResolvedProfile};
use crate::commands::package_appimage;
use crate::commands::package_container::{self, ContainerError};
use crate::commands::ship_check::{self, GlibcVersion, LinuxBinaryReport, STEAM_RUNTIME_GLIBC};
use crate::manifest::{AndroidPlatform, Manifest};
use crate::process;
use toml;

#[path = "package_dlss.rs"]
mod package_dlss;
#[path = "package_xess.rs"]
mod package_xess;

#[path = "package_pipeline_caches.rs"]
mod package_pipeline_caches;

#[path = "package_legal.rs"]
pub(crate) mod package_legal;

/// Target platform of a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Platform {
    /// Linux tarball.
    Linux,
    /// Windows zip.
    Windows,
    /// Android APK or AAB.
    Android,
}

/// Output format of a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Android application package.
    Apk,
    /// Android app bundle.
    Aab,
    /// Plain directory (for inspection).
    Dir,
    /// tar.gz.
    Tar,
    /// Linux AppImage (needs `appimagetool` on `PATH` or in `REGIA_APPIMAGETOOL`; ADR-2631).
    #[value(name = "appimage")]
    AppImage,
    /// zip.
    Zip,
    /// macOS app bundle (not built: macOS is testing-only, ADR-264; every platform rejects it).
    App,
}

/// Build profile of a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Profile {
    /// Cargo `dev` profile, debug-signed.
    #[default]
    Dev,
    /// Cargo `shipping` profile, release-signed.
    Shipping,
}

/// Arguments of `regia package`.
#[derive(Debug, Clone, Args)]
pub struct PackageArgs {
    /// Platform to package for.
    #[arg(long, value_enum)]
    pub platform: Platform,
    /// Output format. Android defaults to apk and Windows to zip. On Linux this flag wins,
    /// otherwise `[platforms.linux] formats` is used, falling back to tar when the list is empty.
    #[arg(long, value_enum)]
    pub format: Option<Format>,
    /// Build profile: `dev`, `shipping`, or a `[profiles.<name>]` entry of `Regia.toml` (entry
    /// levels → dependency closure; the package ships only that closure).
    #[arg(
        long = "profile",
        default_value = "dev",
        default_value_if("server", clap::builder::ArgPredicate::IsPresent, "server")
    )]
    pub profile_name: String,
    /// Cargo profile of the resolved `--profile` (set by [`run`], not a command-line flag).
    #[arg(skip)]
    pub profile: Profile,
    /// Cook if needed, print the size report of the files the profile ships and stop (no build,
    /// no package).
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Project directory (contains `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Output directory (default `<project>/dist/<platform>`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Produce the Steam depot layout and steamcmd VDFs from `[steam]` in `Regia.toml`
    /// (specs/24 § 3): `steam_appid.txt`, the Steamworks redistributable, `steamcloud.md`,
    /// `app_build_<appid>.vdf` and `depot_build_<depotid>.vdf` under `<out>/steam`. Publish with
    /// `regia steam upload`.
    #[arg(long, default_value_t = false)]
    pub steam: bool,
    /// Android only: build just these ABIs (repeatable; each must be listed in
    /// `[platforms.android] abis`). Default: every ABI in the manifest. `install-android` passes the
    /// attached device's ABI so a device build does not also compile the emulator ABI.
    #[arg(long = "abi")]
    pub abis: Vec<String>,
    /// Linux only: the newest glibc the game binary may require, or `host`. Shipping and `--steam`
    /// packages default to `2.31`, the glibc of Steam Linux Runtime 3.0 "sniper" (what Steam and the Steam Deck run
    /// native Linux games in), and build with `cargo zigbuild --target <arch>-unknown-linux-gnu.2.31`
    /// (needs `cargo-zigbuild` and `zig` on `PATH`). Dev packages default to `host`: linked against
    /// this machine's glibc, they do not start on older distros or inside the Steam runtime.
    #[arg(long, value_name = "VERSION|host")]
    pub glibc: Option<String>,
    /// Desktop only: pre-built pipeline caches (loose `.fpc`, stored feedback ZIPs, or a
    /// directory containing either recursively) to ship in `content/pipelines/`. Default: `<project>/pipeline-caches` when it
    /// exists. A tester whose GPU and driver match one of them skips the first-launch shader
    /// compile (`regia_render::pipeline_cache`, book: Shipped pipeline caches).
    #[arg(long, value_name = "PATH")]
    pub pipeline_caches: Option<PathBuf>,
    /// Build reference caches (file, stored ZIP or directory; repeatable). Harvested caches
    /// must match a reference's shader hash and wgpu version. Without references, feedback
    /// caches must match this engine's unsalted shader set; loose caches retain their warning.
    #[arg(long = "pipeline-cache-reference", value_name = "PATH")]
    pub pipeline_cache_references: Vec<PathBuf>,
    /// Desktop only: a map the game prepares at first launch (a level source under the project,
    /// repeatable, in order; ADR-8453). Recorded as `prepare_maps` in `build_info.json` (name and
    /// project-relative path), the list the game hands to `regia_render::prepare` (or the
    /// launcher to `regia prepare --scene`) so a cold driver cache is compiled before play.
    #[arg(long = "prepare-scene", value_name = "LEVEL")]
    pub prepare_scenes: Vec<PathBuf>,
    /// Build the dedicated server (spec 76 § 3): the game's `server` feature, no window, render or
    /// audio crate, a content filter, `server.toml`, `bans.ron`. Linux and Windows only. Default
    /// profile: the built-in `server` (shipping); `--profile` takes `dev` or a `target = "server"`
    /// profile.
    #[arg(long, default_value_t = false)]
    pub server: bool,
    /// Print one JSON object on one stdout line at the end: `ok`, `artifact` (the package's path),
    /// `platform`, `profile`, `server`, `lobby_hub`, `dry_run`, `symbols` and `build_info` (paths or null). Every
    /// other line, the build's included, goes to stderr; a failure prints `ok: false` and `error`
    /// (spec 21 § 1, ADR-7217).
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Audit an existing extracted package only; no cook, build, staging or archive writes.
    /// Uses `[package_hygiene]` in the project's Regia.toml for every profile.
    #[arg(long, value_name = "DIR", conflicts_with_all = ["dry_run", "steam", "out"])]
    pub audit_dir: Option<PathBuf>,
    /// Audit a finished package in place (ZIP, APK, AAB, tar, tar.gz, `.fpak`, another file, or a
    /// directory) and print its final-artifact receipt as `final_audit`. Nothing is built,
    /// extracted or written. Uses `[package_hygiene]` for every profile (ADR-8348).
    #[arg(long, value_name = "FILE", conflicts_with_all = ["dry_run", "steam", "out", "audit_dir"])]
    pub audit_artifact: Option<PathBuf>,
    /// With `--audit-artifact`: check this receipt against the artifact's hash, the project policy
    /// and this tool instead of rescanning (`receipt_verified`).
    #[arg(long, value_name = "RECEIPT", requires = "audit_artifact")]
    pub verify_receipt: Option<PathBuf>,
    /// Linux only: build the binary inside a build container whose glibc is the floor (C-26,
    /// ADR-8835): `steam-sniper` (Steam Linux Runtime 3.0 SDK, glibc 2.31) or `debian12` (glibc
    /// 2.36). Needs podman or docker (`REGIA_CONTAINER_TOOL` picks one); the base image is pinned
    /// by digest, the sources are mounted read-only and the build uses
    /// `<target>/container/<profile>/`. Instead of cargo-zigbuild; `--glibc`, when given, must
    /// equal the profile's glibc.
    #[arg(long, value_enum, value_name = "PROFILE")]
    pub container: Option<package_container::ContainerProfile>,
    /// With `--container`: run the build in this image (it must have cargo) instead of the
    /// toolchain image generated from the profile's pinned base. Pinned by digest
    /// (`<image>@sha256:<hex>`) or a local image with a version tag (`localhost/<name>:<tag>`).
    #[arg(long, value_name = "IMAGE", requires = "container")]
    pub container_image: Option<String>,
    /// With `--container`: another directory to mount read-only at its host path (repeatable), for
    /// path dependencies outside the cargo workspace.
    #[arg(long = "container-mount", value_name = "DIR", requires = "container")]
    pub container_mounts: Vec<PathBuf>,
    /// Leave the default "Made with Regia Engine" line out of the package's `CREDITS.txt` (also
    /// `[package] engine_credit = false` in `Regia.toml`). The credit is a default, not a licence
    /// condition; the MPL notice and the source-location line are always written (ADR-10225).
    #[arg(long, default_value_t = false)]
    pub no_engine_credit: bool,
}

/// The `--json` object of a finished `regia package` (spec 21 § 1, ADR-7217): the package's path as
/// `artifact` (absolute), what was packaged, and the symbols directory and `build_info.json` beside
/// or inside it when they exist.
pub fn json_result(args: &PackageArgs, artifact: &Path) -> serde_json::Value {
    let artifact = std::path::absolute(artifact).unwrap_or_else(|_| artifact.to_path_buf());
    let symbols = artifact
        .parent()
        .map(|dir| dir.join("symbols"))
        .filter(|dir| dir.is_dir());
    let build_info = Some(artifact.join("build_info.json")).filter(|file| file.is_file());
    let path = |p: Option<PathBuf>| p.map(|p| p.display().to_string());
    serde_json::json!({
        "ok": true,
        "artifact": artifact.display().to_string(),
        "platform": build_profile::platform_name(args.platform),
        "profile": args.profile_name,
        "server": args.server,
        "lobby_hub": args.server,
        "dry_run": args.dry_run,
        "symbols": path(symbols),
        "build_info": path(build_info),
        "lint": lint_status(&args.project),
        "final_audit": final_audit_json(&args.project, &artifact),
    })
}

fn lint_status(project: &Path) -> serde_json::Value {
    let path = project.join("build").join("source-lint").join("status.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            serde_json::json!({
                "status": "failed",
                "report": serde_json::Value::Null
            })
        }),
        Err(_) => serde_json::json!({
            "status": "skipped",
            "report": serde_json::Value::Null
        }),
    }
}

/// `final_audit` of a finished package: the receipt beside the artifact plus whether it still
/// verifies against the artifact and policy, or null when none was written (dev profiles,
/// `--dry-run`). Shipping packages always write one (ADR-8348).
fn final_audit_json(project: &Path, artifact: &Path) -> serde_json::Value {
    use regia_cli::package_hygiene as hygiene;
    let sidecar = hygiene::receipt_path(artifact);
    let Ok(text) = std::fs::read_to_string(&sidecar) else {
        return serde_json::Value::Null;
    };
    let Ok(mut receipt) = serde_json::from_str::<serde_json::Value>(&text) else {
        return serde_json::json!({ "verified": false, "receipt_path": sidecar.display().to_string() });
    };
    let verified = hygiene::Policy::from_project(project)
        .and_then(|policy| hygiene::verify_receipt(artifact, &text, &policy))
        .is_ok();
    if let Some(object) = receipt.as_object_mut() {
        object.insert("verified".into(), verified.into());
        object.insert("receipt_path".into(), sidecar.display().to_string().into());
    }
    receipt
}

/// `regia package --server` for a platform that has no server build. `main` turns this into exit
/// code 2. `macos` is no [`Platform`] at all, so `main` refuses that spelling before parsing.
#[derive(Debug, thiserror::Error)]
#[error(
    "server_platform_unsupported: `--server` builds Linux and Windows servers only, not {0} \
     (Android keeps listen servers, macOS is testing-only; spec 76 § 13)"
)]
pub struct ServerPlatformUnsupported(pub &'static str);

/// Runs `regia package` and prints the path of the produced package.
pub fn run(args: &PackageArgs) -> Result<PathBuf> {
    if let Some(staging) = &args.audit_dir {
        let report = audit_existing(args)?;
        outln!("{report}");
        return Ok(staging.clone());
    }
    if let Some(artifact) = &args.audit_artifact {
        let report = audit_artifact_only(args)?;
        outln!("{report}");
        return Ok(artifact.clone());
    }
    if args.server && args.platform == Platform::Android {
        return Err(ServerPlatformUnsupported(build_profile::platform_name(args.platform)).into());
    }
    // `--container` is checked before the project is read or anything is cooked (ADR-8835).
    if let Some(profile) = args.container {
        if args.platform != Platform::Linux {
            return Err(ContainerError::new(
                package_container::CONTAINER_PLATFORM_UNSUPPORTED,
                format!(
                    "--container {} builds Linux binaries; package {} without it",
                    profile.name(),
                    build_profile::platform_name(args.platform)
                ),
            )
            .into());
        }
        package_container::validate(&container_request(args, profile))?;
    }
    let mut args = args.clone();
    let args = &mut args;
    let project_dir = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    // Validate through the redacting reader before generic TOML diagnostics can quote policy data.
    regia_cli::package_hygiene::Policy::from_project(&project_dir)?;
    regia_cli::package_hygiene::StageRules::from_project(&project_dir)?;
    let manifest = Manifest::load(&project_dir)?;
    let mut resolved = build_profile::resolve(&manifest, &args.profile_name)?;
    // `--server --profile dev`: a dev build of the server.
    if args.server && matches!(args.profile_name.as_str(), "dev" | "shipping") {
        resolved.server = true;
    }
    if args.server && !resolved.server {
        bail!(
            "--server needs a server profile: `[profiles.{}]` has no `target = \"server\"`",
            args.profile_name
        );
    }
    if resolved.server && args.platform == Platform::Android {
        return Err(ServerPlatformUnsupported(build_profile::platform_name(args.platform)).into());
    }
    if resolved.server {
        args.server = true;
    }
    args.profile = resolved.cargo;
    let args = &*args;
    // The engine notice's settings, checked before the (long) cook; a release build still pointing at
    // the placeholder source host is told so (MPL-2.0 § 3.2, ADR-10225).
    let legal = package_legal::Legal::resolve(&project_dir, args.no_engine_credit)?;
    if let Some(warning) = package_legal::release_warning(&legal, args.profile == Profile::Shipping) {
        tracing::warn!("{warning}");
    }
    if args.server && args.steam {
        bail!("--steam is a client package option; Steam server registration comes with the Steam transport");
    }

    if args.steam && !matches!(args.platform, Platform::Linux | Platform::Windows) {
        bail!("--steam is a desktop option (specs/24 § 3): package for `linux` or `windows`");
    }
    if args.glibc.is_some() && args.platform != Platform::Linux {
        bail!("--glibc applies to Linux packages only");
    }
    if args.platform == Platform::Linux {
        // Validate `--glibc` before the (long) cook and build.
        linux_glibc_floor(args)?;
    }
    // Check optional vendor redistributables before cooking or compiling the game.
    let dlss = package_dlss::Plan::from_env(args.platform, args.server)?;
    let xess = package_xess::Plan::from_env(args.platform, args.server)?;
    // Auto-cook: cook the platform first when paks are missing or stale.
    ensure_paks(&project_dir, &manifest, args)?;
    // The shader-permutation budget per render profile (master plan § 5.5a, ADR-2026): over
    // budget fails a shipping package and warns on a dev one. A dedicated server has no renderer.
    if !args.server {
        ship_check::check_permutation_budget(
            &project_dir,
            build_profile::platform_name(args.platform),
            args.profile == Profile::Shipping,
        )?;
        // The shipped-shader bytes row and the platforms the project does not ship (ADR-2664).
        ship_check::check_permutation_platforms(
            &project_dir,
            build_profile::platform_name(args.platform),
            args.profile == Profile::Shipping,
        )?;
    }

    if args.dry_run {
        out!("{}", build_profile::dry_run(&project_dir, &resolved, args)?);
        return Ok(args.out.clone().unwrap_or_else(|| {
            let dir = platform_dir_name(args.platform);
            if args.server {
                default_out_dir(&project_dir, &format!("server-{dir}"))
            } else {
                default_out_dir(&project_dir, dir)
            }
        }));
    }

    if args.server {
        return crate::commands::server_package::package(&project_dir, &manifest, args, &resolved);
    }
    match args.platform {
        Platform::Android => package_android(&project_dir, &manifest, args),
        Platform::Linux => package_linux(&project_dir, &manifest, args, dlss.as_ref()),
        Platform::Windows => package_windows(&project_dir, &manifest, args, dlss.as_ref(), xess.as_ref()),
    }
}

/// Audit a package directory without starting any build or changing the directory.
///
/// # Errors
/// Missing audit directory, invalid policy, incomplete scan, or rejected package.
pub fn audit_existing(args: &PackageArgs) -> Result<serde_json::Value> {
    let staging = args.audit_dir.as_deref().context("--audit-dir is required")?;
    let report = regia_cli::package_hygiene::check_project_tree(&args.project, staging)?;
    Ok(serde_json::json!({ "ok": true, "audit_only": true, "hygiene": report }))
}

/// Audit (or, with `--verify-receipt`, verify) a finished package without building or writing.
///
/// # Errors
/// Invalid policy, unreadable artifact, malformed archive, incomplete scan, rejected package,
/// uninspected content under a strict policy, or a receipt that no longer matches.
pub fn audit_artifact_only(args: &PackageArgs) -> Result<serde_json::Value> {
    use regia_cli::package_hygiene as hygiene;
    let artifact = args
        .audit_artifact
        .as_deref()
        .context("--audit-artifact is required")?;
    let policy = hygiene::Policy::from_project(&args.project)?;
    if let Some(receipt) = &args.verify_receipt {
        let text = std::fs::read_to_string(receipt).map_err(|_| hygiene_io("receipt"))?;
        hygiene::verify_receipt(artifact, &text, &policy)?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| hygiene_io("receipt"))?;
        return Ok(serde_json::json!({
            "ok": true,
            "audit_only": true,
            "receipt_verified": true,
            "final_audit": value,
        }));
    }
    let receipt = hygiene::check_artifact(artifact, &policy, &hygiene::ArtifactOptions::default())?;
    Ok(serde_json::json!({ "ok": true, "audit_only": true, "final_audit": receipt }))
}

/// A redacted `package_hygiene_io` error for a fixed location label.
fn hygiene_io(location: &str) -> anyhow::Error {
    regia_cli::package_hygiene::AuditError {
        code: "package_hygiene_io",
        location: location.to_owned(),
        location_id: None,
        report: None,
        matcher_bytes: 0,
        matchers_compiled: 0,
    }
    .into()
}

/// The final-artifact hook (ADR-8348): after an archive (or the dir format) is produced, a
/// shipping package audits the generated artifact itself and writes its receipt beside it.
/// A stale receipt is always removed first, so a dev or failed package never keeps one.
/// `prefixes` are the in-archive roots stripped to match staging-relative allowances.
///
/// # Errors
/// Invalid policy, malformed archive, rejected or (under a strict policy) uninspected content,
/// or a receipt that cannot be written. The artifact itself is never modified.
pub(crate) fn final_artifact_hook(
    project: &Path,
    artifact: &Path,
    profile: Profile,
    prefixes: &[String],
) -> Result<()> {
    use regia_cli::package_hygiene as hygiene;
    hygiene::remove_receipt(artifact)?;
    if profile != Profile::Shipping {
        return Ok(());
    }
    let policy = hygiene::Policy::from_project(project)?;
    let options = hygiene::ArtifactOptions {
        allow_prefixes: prefixes.to_vec(),
        spool_dir: None,
    };
    let receipt = hygiene::check_artifact(artifact, &policy, &options)?;
    let path = hygiene::write_receipt(artifact, &receipt)?;
    outln!(
        "package hygiene (final {}): {} archive entries, {} pak entries, {} containers, {} uninspected, receipt {}",
        receipt.artifact_kind,
        receipt.coverage.archive_entries,
        receipt.hygiene.pak_entries_scanned,
        receipt.coverage.containers.len(),
        receipt.coverage.uninspected.len(),
        path.display()
    );
    Ok(())
}

/// Enforce the project hygiene policy before publishing a shipping staging tree.
/// Dev packages keep their development strings; `--audit-dir` always enforces the policy.
///
/// # Errors
/// Invalid policy, incomplete scan, or rejected package.
pub(crate) fn check_package_hygiene(project: &Path, staging: &Path, profile: Profile) -> Result<()> {
    if profile == Profile::Shipping {
        let prepared = regia_cli::package_hygiene::prepare_project_tree(project, staging)?;
        let report = regia_cli::package_hygiene::check_project_tree(project, staging)?;
        outln!(
            "package hygiene: {} files, {} pak entries, {} reviewed exceptions, {} stage actions",
            report.files_scanned,
            report.pak_entries_scanned,
            report.allowed_findings,
            prepared.actions.len()
        );
    }
    Ok(())
}

/// Clears a stale staging dir from a previous package and recreates it with a `content` dir.
/// `stage_content` only clears `content` and `assets`, so anything else (an old binary,
/// `steam_appid.txt`, a removed icon) would survive into the new archive. Mirror the server
/// packager (server_package.rs).
///
/// # Errors
///
/// The stale dir cannot be removed, or the `content` dir cannot be created.
fn fresh_staging(staging: &Path) -> Result<()> {
    if staging.exists() {
        std::fs::remove_dir_all(staging)?;
    }
    std::fs::create_dir_all(staging.join("content"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

/// Package layout per specs/21 § 4:
/// `dist/linux/<Name>/{<name> (binary), content/*.fpak, <name>.desktop, icon.png}` → tar.gz.
fn package_linux(
    project_dir: &Path,
    manifest: &Manifest,
    args: &PackageArgs,
    dlss: Option<&package_dlss::Plan>,
) -> Result<PathBuf> {
    let formats = linux_formats(args, manifest)?;
    // Fail on a missing appimagetool before the (long) build, never after it.
    let appimagetool = if formats.contains(&Format::AppImage) {
        Some(package_appimage::require_appimagetool()?)
    } else {
        None
    };

    let slug = staging_slug(manifest);
    let target_pkg = cargo_package_name(project_dir)?;
    let package = target_pkg.package.as_str();
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| default_out_dir(project_dir, "linux"));
    let staging = out_dir.join(&slug);
    fresh_staging(&staging)?;

    // Build the binary: in a build container (`--container`), against the glibc floor
    // (cargo-zigbuild) or the host's glibc.
    let glibc = linux_glibc_floor(args)?;
    let resolved = build_profile::resolve(manifest, &args.profile_name)?;
    let (binary, target) = match args.container {
        Some(profile) => {
            let call = container_host_call(project_dir, package, &target_pkg.bin, &resolved, profile)?;
            let binary = package_container::build(&container_request(args, profile), call, on_path)?;
            if !binary.is_file() {
                bail!("the container build wrote no {}", binary.display());
            }
            (binary, None)
        }
        None => {
            let target = glibc.map(linux_glibc_target);
            build_cargo(project_dir, package, &resolved, target.as_deref())?;
            let binary = find_binary(
                project_dir,
                &target_pkg.bin,
                args.profile,
                false,
                target.as_deref().map(cargo_target_dir_name),
            )?;
            (binary, target)
        }
    };
    let bin_name = binary
        .file_name()
        .expect("binary path must have a filename")
        .to_string_lossy()
        .into_owned();
    std::fs::copy(&binary, staging.join(&bin_name))?;

    // Keep the separate debug symbols for symbolication (specs/21 § 4.4).
    keep_shipping_symbols(
        &binary,
        &staging.join(&bin_name),
        &out_dir,
        args.profile,
        SymbolFormat::Dwarf,
    )?;

    // The staged binary must start in the target runtime and carry nothing of the build machine.
    let elf_report = check_staged_linux_binary(
        &staging.join(&bin_name),
        project_dir,
        args.profile,
        glibc,
        &resolved,
    )?;

    // Copy paks (the profile's closure, or everything) and run ship_check over the staged content.
    stage_content(
        project_dir,
        "linux",
        &resolved,
        &staging,
        &staging.join("content"),
        &staging.join("assets"),
        || stage_raw_assets(project_dir, &staging.join("assets"), args.profile, "linux"),
    )?;
    let shipped_caches = stage_pipeline_caches(project_dir, args, &staging.join("content"))?;

    // Copy raw (non-cookable) assets and the third-party notices so the shipped
    // binary is self-contained: `assets://about.txt` resolves from `<exe dir>/assets`.
    copy_notices(
        &staging,
        project_dir,
        &sbom_triples(target.as_deref()),
        args.no_engine_credit,
    )?;
    if let Some(dlss) = dlss {
        dlss.stage(&staging)?;
    }

    // Write .desktop file.
    let desktop = desktop_entry(&manifest.project.name, &bin_name)?;
    std::fs::write(staging.join(format!("{slug}.desktop")), desktop)?;

    // Write icon if present.
    if let Some(icon_path) = icon_for_project(project_dir, manifest) {
        let dest = staging.join("icon.png");
        std::fs::copy(&icon_path, &dest)?;
    }

    // Write build_info.json.
    let mut extra = serde_json::Map::new();
    extra.insert(
        "glibc_floor".into(),
        glibc.map_or(serde_json::Value::from("host"), |v| v.to_string().into()),
    );
    if let Some(max) = elf_report.as_ref().and_then(|r| r.max_glibc) {
        extra.insert("glibc_max".into(), max.to_string().into());
    }
    extra.insert("pipeline_caches".into(), shipped_caches.into());
    extra.insert(
        "prepare_maps".into(),
        prepare_maps(project_dir, &args.prepare_scenes)?,
    );
    insert_render_profile(&mut extra, manifest, "linux");
    write_build_info(&staging, project_dir, manifest, extra)?;

    // The Steam depot layout + VDFs (specs/24 § 3) finish the staged package.
    if args.steam {
        let steam_dir =
            crate::commands::steam::finish_package(manifest, "linux", &staging, package, &out_dir)?;
        outln!("steam: {}", steam_dir.display());
        if let Some(actions) =
            crate::commands::steam::package_input_manifest(project_dir, manifest, &steam_dir)?
        {
            outln!("steam input: {}", actions.display());
        }
    }

    check_package_hygiene(project_dir, &staging, args.profile)?;

    let mut produced: Vec<PathBuf> = Vec::new();
    for fmt in &formats {
        match fmt {
            Format::Dir => {
                final_artifact_hook(project_dir, &staging, args.profile, &[])?;
                produced.push(staging.clone());
            }
            Format::Tar => {
                let tarball = out_dir.join(format!("{slug}-linux.tar.gz"));
                write_tarball(&tarball, &out_dir, &slug)?;
                final_artifact_hook(project_dir, &tarball, args.profile, &[format!("{slug}/")])?;
                produced.push(tarball);
            }
            Format::AppImage => {
                let Some(tool) = appimagetool.as_deref() else {
                    bail!("appimage_failed: appimagetool was not resolved");
                };
                let icon = icon_for_project(project_dir, manifest).ok_or_else(|| {
                    anyhow::anyhow!(
                        "appimage_icon_missing: an AppImage needs an icon; add a PNG at assets/branding/icon.png"
                    )
                })?;
                let appdir = out_dir.join(format!("{slug}.AppDir"));
                package_appimage::build_appdir(
                    &staging,
                    &appdir,
                    &slug,
                    &bin_name,
                    &manifest.project.name,
                    &manifest.project.version,
                    &icon,
                )?;
                let image = out_dir.join(format!("{slug}-{}.AppImage", package_appimage::appimage_arch()));
                package_appimage::run_appimagetool(tool, &appdir, &image)?;
                final_artifact_hook(project_dir, &image, args.profile, &[])?;
                produced.push(image);
            }
            other => bail!("--format {} is not supported for Linux", format_name(*other)),
        }
    }
    for path in &produced {
        outln!("{}", path.display());
    }
    produced
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no Linux package format selected"))
}

/// The Linux formats to build: `--format`, else `[platforms.linux] formats`, else `tar`.
fn linux_formats(args: &PackageArgs, manifest: &Manifest) -> Result<Vec<Format>> {
    if let Some(format) = args.format {
        return match format {
            Format::Tar | Format::Dir | Format::AppImage => Ok(vec![format]),
            other => bail!(
                "--format {} is not supported for Linux; use `tar`, `dir` or `appimage`",
                format_name(other)
            ),
        };
    }
    let mut formats = Vec::new();
    for name in &manifest.platforms.linux.formats {
        let format = match name.as_str() {
            "tar" => Format::Tar,
            "dir" => Format::Dir,
            "appimage" => Format::AppImage,
            other => bail!("linux_format_unknown: [platforms.linux] formats entry `{other}`"),
        };
        if !formats.contains(&format) {
            formats.push(format);
        }
    }
    if formats.is_empty() {
        formats.push(Format::Tar);
    }
    Ok(formats)
}

/// `tar -czf <tarball> -C <parent> <dir>`, replacing an old archive.
///
/// Writes to `<tarball>.part` and renames it into place on success, so a mid-run failure (disk full,
/// a staged file deleted or changed as it is read, `tar` killed) never leaves a truncated archive
/// under the final name and never destroys the previous good one — the same contract as
/// [`write_zip`].
pub(crate) fn write_tarball(tarball: &Path, parent: &Path, dir: &str) -> Result<()> {
    // Only a staging directory that exists must carry the SBOM; a missing one is `tar`'s error below.
    let staged = parent.join(dir);
    if staged.is_dir() && !staged.join("sbom.json").is_file() {
        bail!("package SBOM missing from archive staging directory");
    }
    // A sibling temporary name in the same directory, so the rename below is atomic.
    let part = PathBuf::from(format!("{}.part", tarball.display()));
    // Remove a stale partial left by a previous failed run.
    let _ = std::fs::remove_file(&part);
    process::run(
        Command::new("tar").args([
            "-czf",
            &part.to_string_lossy(),
            "-C",
            &parent.to_string_lossy(),
            dir,
        ]),
        "Install tar (usually preinstalled)",
    )
    .context("creating the tarball")?;
    std::fs::rename(&part, tarball)
        .with_context(|| format!("moving {} into place as {}", part.display(), tarball.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// MSVC target a Linux host cross-compiles Windows packages to (specs/21 § 4, cargo-xwin).
const WINDOWS_MSVC_TARGET: &str = "x86_64-pc-windows-msvc";

/// Cross target for `platform` when this host cannot build it natively.
///
/// Linux → Windows uses `x86_64-pc-windows-msvc`. A Windows host builds the exe with the
/// host toolchain and returns `None`.
pub(crate) fn cross_target(platform: Platform) -> Option<&'static str> {
    match platform {
        Platform::Windows if !cfg!(windows) => Some(WINDOWS_MSVC_TARGET),
        _ => None,
    }
}

/// `cargo xwin` supplies the MSVC CRT and SDK. A plain `cargo build --target
/// x86_64-pc-windows-msvc` has no sysroot on Linux (specs/21 § 4).
fn use_cargo_xwin(target: Option<&str>) -> bool {
    !cfg!(windows) && target.is_some_and(|triple| triple.contains("pc-windows-msvc"))
}

/// `cargo-xwin` is a cargo subcommand (`cargo xwin …`) when `cargo-xwin` is on `PATH`.
fn cargo_xwin_installed() -> bool {
    on_path("cargo-xwin")
}

/// True when `tool` (or `tool.exe`) is a file in a `PATH` directory.
fn on_path(tool: &str) -> bool {
    let Some(path) = regia_core::brand::env_var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .any(|dir| dir.join(tool).is_file() || dir.join(format!("{tool}.exe")).is_file())
}

/// The cargo front end that builds a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildTool {
    /// Plain `cargo build` (host toolchain, host libc).
    Cargo,
    /// `cargo xwin build`: Linux → Windows MSVC cross build.
    Xwin,
    /// `cargo zigbuild`: Linux against an older glibc (`<triple>.<glibc>`), zig as the linker.
    Zigbuild,
}

/// Picks the build tool for `target`: cargo-xwin for MSVC from a non-Windows host, cargo-zigbuild
/// for a glibc-suffixed Linux triple (`x86_64-unknown-linux-gnu.2.31`), cargo otherwise.
fn build_tool(target: Option<&str>) -> BuildTool {
    if use_cargo_xwin(target) {
        BuildTool::Xwin
    } else if target.is_some_and(|t| t != cargo_target_dir_name(t)) {
        BuildTool::Zigbuild
    } else {
        BuildTool::Cargo
    }
}

/// The directory cargo writes a target's artefacts to: the triple without cargo-zigbuild's glibc
/// suffix (`x86_64-unknown-linux-gnu.2.31` → `x86_64-unknown-linux-gnu`).
pub(crate) fn cargo_target_dir_name(target: &str) -> &str {
    match target.split_once("-gnu.") {
        Some((triple, _glibc)) if target.contains("-linux-") => &target[..triple.len() + "-gnu".len()],
        _ => target,
    }
}

/// The glibc floor of a Linux package: `--glibc <VERSION>`, `--glibc host` (none), or the default
/// ([`STEAM_RUNTIME_GLIBC`] for shipping and `--steam` packages, the host's glibc for dev).
pub(crate) fn linux_glibc_floor(args: &PackageArgs) -> Result<Option<GlibcVersion>> {
    if let Some(profile) = args.container {
        return container_glibc_floor(args, profile).map(Some);
    }
    let floor = match args.glibc.as_deref() {
        Some("host") => None,
        Some(text) => Some(GlibcVersion::parse(text).with_context(|| {
            format!("--glibc {text:?}: expected a version such as `{STEAM_RUNTIME_GLIBC}` or `host`")
        })?),
        None if args.profile == Profile::Shipping || args.steam => Some(STEAM_RUNTIME_GLIBC),
        None => None,
    };
    if args.steam && floor.is_none_or(|v| v > STEAM_RUNTIME_GLIBC) {
        bail!(
            "--steam Linux depots must run in Steam Linux Runtime 3.0 \"sniper\" (glibc {STEAM_RUNTIME_GLIBC}): \
             drop `--glibc` or pass `--glibc {STEAM_RUNTIME_GLIBC}` (or older)"
        );
    }
    Ok(floor)
}

/// The glibc floor of a `--container` package: the profile's glibc. `--glibc`, when given, must say
/// the same, and a `--steam` depot needs a profile at or below the Steam runtime's glibc.
fn container_glibc_floor(
    args: &PackageArgs,
    profile: package_container::ContainerProfile,
) -> Result<GlibcVersion> {
    let floor = profile.glibc();
    if let Some(text) = args.glibc.as_deref()
        && GlibcVersion::parse(text) != Some(floor)
    {
        return Err(ContainerError::new(
            package_container::CONTAINER_GLIBC_CONFLICT,
            format!(
                "--glibc {text} disagrees with --container {} (glibc {floor}): drop --glibc, or pick the profile \
                 with that glibc",
                profile.name()
            ),
        )
        .into());
    }
    if args.steam && floor > STEAM_RUNTIME_GLIBC {
        return Err(ContainerError::new(
            package_container::CONTAINER_GLIBC_CONFLICT,
            format!(
                "--steam Linux depots run in Steam Linux Runtime 3.0 \"sniper\" (glibc {STEAM_RUNTIME_GLIBC}), \
                 but --container {} builds for glibc {floor}: use --container steam-sniper",
                profile.name()
            ),
        )
        .into());
    }
    Ok(floor)
}

/// The container request of `args` (`--container`, `--container-image`, `--container-mount`).
fn container_request(
    args: &PackageArgs,
    profile: package_container::ContainerProfile,
) -> package_container::ContainerRequest<'_> {
    package_container::ContainerRequest {
        profile,
        image: args.container_image.as_deref(),
        mounts: &args.container_mounts,
    }
}

/// The cargo-zigbuild triple for this host's architecture with the glibc floor appended.
pub(crate) fn linux_glibc_target(glibc: GlibcVersion) -> String {
    format!("{}-unknown-linux-gnu.{glibc}", std::env::consts::ARCH)
}

/// Checks the staged (stripped) Linux binary: the glibc floor and runtime libraries when the package
/// targets one, and no build-machine strings in shipping builds. Returns the ELF report when a glibc
/// floor was checked.
pub(crate) fn check_staged_linux_binary(
    staged: &Path,
    project_dir: &Path,
    profile: Profile,
    glibc: Option<GlibcVersion>,
    resolved: &ResolvedProfile,
) -> Result<Option<LinuxBinaryReport>> {
    let report = match glibc {
        Some(floor) => {
            let report = ship_check::check_linux_binary(staged, floor)?;
            outln!(
                "glibc: binary needs {} (floor {floor}); NEEDED: {}",
                report
                    .max_glibc
                    .map_or_else(|| "none".to_string(), |v| format!("GLIBC_{v}")),
                report.needed.join(", ")
            );
            if !report.outside_runtime.is_empty() {
                tracing::warn!(
                    "{} links {} which Steam Linux Runtime 3.0 does not provide; bundle them next to \
                     the binary (with an `$ORIGIN` rpath) or load them with dlopen",
                    staged.display(),
                    report.outside_runtime.join(", ")
                );
            }
            Some(report)
        }
        None => None,
    };
    if profile == Profile::Shipping {
        let workspace = cargo_workspace_root(project_dir)?;
        ship_check::check_no_build_machine_strings(staged, &workspace)?;
        ship_check::check_binary_features(
            staged,
            resolved.allow_agent(),
            resolved.spec.is_some() || resolved.server,
        )?;
    }
    Ok(report)
}

/// Arguments of the cargo invocation that builds one package.
///
/// `tool` picks the front end (`cargo build`, `cargo xwin build`, `cargo zigbuild`); `config` adds
/// `--config` overrides (shipping path remapping).
fn cargo_build_args(
    package: &str,
    profile: Profile,
    target: Option<&str>,
    tool: BuildTool,
    config: &[String],
) -> Vec<String> {
    let profile_arg = match profile {
        Profile::Dev => "dev",
        Profile::Shipping => "shipping",
    };
    let mut args = Vec::new();
    match tool {
        BuildTool::Cargo => args.push("build".to_string()),
        BuildTool::Xwin => {
            args.push("xwin".to_string());
            args.push("build".to_string());
        }
        BuildTool::Zigbuild => args.push("zigbuild".to_string()),
    }
    args.push("--profile".to_string());
    args.push(profile_arg.to_string());
    if let Some(triple) = target {
        args.push("--target".to_string());
        args.push(triple.to_string());
    }
    args.push("-p".to_string());
    args.push(package.to_string());
    for value in config {
        args.push("--config".to_string());
        args.push(value.clone());
    }
    args
}

/// `--remap-path-prefix` flags that keep the build machine's directories out of a shipping binary
/// (panic locations and debug info name `/regia/…`, `/cargo/…`, `/target/…` instead), as one
/// `--config target.<triple>.rustflags=[…]` value. Cargo joins it with the config files' rustflags
/// for that target (and with cargo-xwin's / cargo-zigbuild's), so nothing they set is lost.
///
/// rustc applies the last matching prefix, so the most specific directories come last.
///
/// Linux binaries also get an `$ORIGIN` rpath: Steam does not put the game directory on the library
/// path, and `libsteam_api.so` (and any bundled library) ships next to the binary (specs/24 § 3).
///
/// The home directory is `HOME`, else `USERPROFILE`: a Windows host sets only the latter, and without
/// it a Windows-built shipping exe kept `C:\Users\<name>\.cargo\registry\…` panic paths (ADR-868).
/// The home remap target is `/builder`, never `/home`: a `/home/…` prefix would itself match the
/// packaging host-string gate's private-path patterns (ADR-10354).
fn remap_config(target_triple: &str, workspace: &Path, target_dir: &Path) -> String {
    let home = crate::android::toolchain::home_dir();
    let cargo_home = regia_core::brand::env_var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".cargo")));
    remap_config_paths(
        target_triple,
        home.as_deref(),
        cargo_home.as_deref(),
        workspace,
        target_dir,
    )
}

/// [`remap_config`] with the home and cargo-home directories given (a container build's cargo
/// home is its per-profile directory, not this machine's).
fn remap_config_paths(
    target_triple: &str,
    home: Option<&Path>,
    cargo_home: Option<&Path>,
    workspace: &Path,
    target_dir: &Path,
) -> String {
    let mut maps: Vec<(PathBuf, &str)> = Vec::new();
    if let Some(home) = home {
        maps.push((home.to_path_buf(), "/builder"));
    }
    if let Some(cargo_home) = cargo_home {
        maps.push((cargo_home.to_path_buf(), "/cargo"));
    }
    maps.push((workspace.to_path_buf(), "/regia"));
    maps.push((target_dir.to_path_buf(), "/target"));
    let mut flags: Vec<String> = maps
        .iter()
        .map(|(from, to)| format!("--remap-path-prefix={}={to}", from.display()))
        .collect();
    if target_triple.contains("-linux-") {
        flags.push("-Clink-arg=-Wl,-rpath,$ORIGIN".to_string());
    }
    let flags: Vec<String> = flags
        .iter()
        .map(|flag| format!("\"{}\"", flag.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("target.{target_triple}.rustflags=[{}]", flags.join(", "))
}

/// The target triples a package's SBOM is filtered to: the cross target its build used (a
/// cargo-zigbuild glibc suffix removed), else this host's triple, else none (then the SBOM is not
/// filtered).
pub(crate) fn sbom_triples(target: Option<&str>) -> Vec<String> {
    match target {
        Some(target) => vec![cargo_target_dir_name(target).to_owned()],
        None => host_triple().into_iter().collect(),
    }
}

/// The host's target triple (`rustc -vV`), for the host build's `--config target.<triple>` key.
fn host_triple() -> Option<String> {
    let out = process::try_output(Command::new("rustc").arg("-vV"))?;
    out.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(|t| t.trim().to_string())
}

/// The git revision of the project's source tree: `REGIA_GIT_HASH` when set (build machines that
/// receive an exported tree without `.git`), else `git rev-parse` in the project, else `unknown`.
fn git_revision(project_dir: &Path) -> String {
    if let Some(hash) = regia_core::brand::env_var("REGIA_GIT_HASH")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
    {
        return hash;
    }
    process::try_output(
        Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(project_dir),
    )
    .map_or_else(|| "unknown".to_string(), |hash| hash.trim().to_string())
}

/// `dist/windows/<Name>/{<Name>.exe, content/*.fpak}` → zip.
fn package_windows(
    project_dir: &Path,
    manifest: &Manifest,
    args: &PackageArgs,
    dlss: Option<&package_dlss::Plan>,
    xess: Option<&package_xess::Plan>,
) -> Result<PathBuf> {
    let target = cross_target(Platform::Windows);
    if target.is_some() {
        tracing::warn!(
            "cross-compiling the Windows package with cargo-xwin ({WINDOWS_MSVC_TARGET}); the exe is not run on this host"
        );
    }
    let fmt = args.format.unwrap_or(Format::Zip);
    match fmt {
        Format::Zip | Format::Dir => {}
        other => bail!(
            "--format {} is not supported for Windows; use `zip` or `dir`",
            format_name(other)
        ),
    }

    let slug = staging_slug(manifest);
    let target_pkg = cargo_package_name(project_dir)?;
    let package = target_pkg.package.as_str();
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| default_out_dir(project_dir, "windows"));
    let staging = out_dir.join(&slug);
    fresh_staging(&staging)?;

    // Build the binary (cargo-xwin on a Linux host; the host toolchain on Windows).
    let resolved = build_profile::resolve(manifest, &args.profile_name)?;
    build_cargo(project_dir, package, &resolved, target)?;
    let binary = find_binary(project_dir, &target_pkg.bin, args.profile, true, target)?;
    let exe_name = format!("{}.exe", target_pkg.bin);
    std::fs::copy(&binary, staging.join(&exe_name))?;

    // Keep the separate debug symbols for symbolication (specs/21 § 4.4).
    keep_shipping_symbols(
        &binary,
        &staging.join(&exe_name),
        &out_dir,
        args.profile,
        SymbolFormat::Pdb,
    )?;
    if args.profile == Profile::Shipping {
        let workspace = cargo_workspace_root(project_dir)?;
        ship_check::check_no_build_machine_strings(&staging.join(&exe_name), &workspace)?;
        ship_check::check_binary_features(
            &staging.join(&exe_name),
            resolved.allow_agent(),
            resolved.spec.is_some(),
        )?;
        // A GUI-subsystem exe opens no console window (ADR-2306).
        ship_check::check_windows_subsystem(&staging.join(&exe_name), resolved.spec.is_some())?;
    }
    // `manifest.project.version` is the serde default ("0.1.0") when Regia.toml sets no
    // `[project] version`, and the templates' build.rs then embeds CARGO_PKG_VERSION: only a
    // manifest that names its version can mismatch the exe's resources.
    let version = project_sets_version(project_dir).then(|| manifest.project.version.clone());
    ship_check::check_windows_resources(
        &staging.join(&exe_name),
        version.as_deref(),
        args.profile == Profile::Shipping && resolved.spec.is_some(),
    )?;

    // Copy paks (the profile's closure, or everything) and run ship_check over the staged content.
    stage_content(
        project_dir,
        "windows",
        &resolved,
        &staging,
        &staging.join("content"),
        &staging.join("assets"),
        || stage_raw_assets(project_dir, &staging.join("assets"), args.profile, "windows"),
    )?;
    let shipped_caches = stage_pipeline_caches(project_dir, args, &staging.join("content"))?;

    // Copy raw assets and notices (self-contained Windows zip).
    copy_notices(
        &staging,
        project_dir,
        &sbom_triples(target),
        args.no_engine_credit,
    )?;
    if let Some(dlss) = dlss {
        dlss.stage(&staging)?;
    }
    if let Some(xess) = xess {
        xess.stage(&staging)?;
    }

    // Write build_info.json.
    let mut extra = serde_json::Map::new();
    extra.insert("pipeline_caches".into(), shipped_caches.into());
    extra.insert(
        "prepare_maps".into(),
        prepare_maps(project_dir, &args.prepare_scenes)?,
    );
    insert_render_profile(&mut extra, manifest, "windows");
    write_build_info(&staging, project_dir, manifest, extra)?;

    // The Steam depot layout + VDFs (specs/24 § 3) finish the staged package.
    if args.steam {
        let steam_dir =
            crate::commands::steam::finish_package(manifest, "windows", &staging, package, &out_dir)?;
        outln!("steam: {}", steam_dir.display());
        if let Some(actions) =
            crate::commands::steam::package_input_manifest(project_dir, manifest, &steam_dir)?
        {
            outln!("steam input: {}", actions.display());
        }
    }

    check_package_hygiene(project_dir, &staging, args.profile)?;

    if fmt == Format::Dir {
        final_artifact_hook(project_dir, &staging, args.profile, &[])?;
        outln!("{}", staging.display());
        return Ok(staging);
    }

    // zip
    let zip_path = out_dir.join(format!("{slug}-windows.zip"));
    write_zip(&zip_path, &out_dir, &staging, &slug)?;
    final_artifact_hook(project_dir, &zip_path, args.profile, &[format!("{slug}/")])?;
    outln!("{}", zip_path.display());
    Ok(zip_path)
}

/// Zips `staging` (the directory `dir` under `out_dir`) into `zip_path`, replacing an old archive.
pub(crate) fn write_zip(zip_path: &Path, out_dir: &Path, staging: &Path, dir: &str) -> Result<()> {
    if !staging.join("sbom.json").is_file() {
        bail!("package SBOM missing from archive staging directory");
    }
    // Remove existing zip if present.
    let _ = std::fs::remove_file(zip_path);
    // Use platform-appropriate zip command.
    let zip_result = if cfg!(windows) {
        process::try_output(
            Command::new("powershell")
                .args([
                    "-Command",
                    &format!(
                        "Compress-Archive -Path {} -DestinationPath {}",
                        powershell_quote(&staging.to_string_lossy()),
                        powershell_quote(&zip_path.to_string_lossy())
                    ),
                ])
                .current_dir(out_dir),
        )
    } else {
        process::try_output(
            Command::new("zip")
                .args(["-r", &zip_path.to_string_lossy(), dir])
                .current_dir(out_dir),
        )
    };
    if zip_result.is_none() {
        bail!(
            "creating {} failed: the `{}` tool is missing or exited with an error. Install it, or run \
             with `--format dir` to get the staged directory",
            zip_path.display(),
            if cfg!(windows) { "powershell" } else { "zip" }
        );
    }
    Ok(())
}

/// The asset packs of the chunked Android cook in `pak_dir` (none without a content manifest).
fn android_asset_packs(pak_dir: &Path) -> Result<Vec<AssetPackModule>> {
    match regia_asset::content::ContentManifest::read_from_dir(pak_dir)
        .map_err(|e| anyhow::anyhow!("{}: {e}", pak_dir.display()))?
    {
        Some(manifest) => gradle_project::asset_pack_modules(&manifest),
        None => Ok(Vec::new()),
    }
}

/// Copies each pack chunk's pak from `pak_dir` into its module's `src/main/assets/` (specs/40 § 2.6).
fn stage_asset_packs(pak_dir: &Path, gradle_dir: &Path, packs: &[AssetPackModule]) -> Result<()> {
    let chunks = if packs.is_empty() {
        Vec::new()
    } else {
        regia_asset::content::ContentManifest::read_from_dir(pak_dir)
            .map_err(|e| anyhow::anyhow!("{}: {e}", pak_dir.display()))?
            .map(|manifest| manifest.chunks)
            .unwrap_or_default()
    };
    for pack in packs {
        // A pack pak must be the one its manifest entry describes (ADR-7413).
        let chunk = chunks.iter().find(|c| c.file == pack.file).with_context(|| {
            format!(
                "asset pack {} ({}) is not in the content manifest",
                pack.pack, pack.file
            )
        })?;
        super::content::check_chunk_file(pak_dir, chunk)?;
        let dest = pack
            .asset_path()
            .split('/')
            .fold(gradle_dir.to_path_buf(), |p, part| p.join(part));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::copy(pak_dir.join(&pack.file), &dest)
            .with_context(|| format!("staging asset pack {} ({})", pack.pack, pack.file))?;
    }
    Ok(())
}

/// A PowerShell single-quoted string literal: embedded `'` are doubled.
fn powershell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

// ---------------------------------------------------------------------------
// Android
// ---------------------------------------------------------------------------

fn package_android(project_dir: &Path, manifest: &Manifest, args: &PackageArgs) -> Result<PathBuf> {
    let manifest = &with_android_abis(manifest, &args.abis)?;
    let fmt = args.format.unwrap_or(Format::Apk);
    match fmt {
        Format::Apk | Format::Aab => {}
        other => bail!(
            "--format {} is not an Android format; use `apk` or `aab`",
            format_name(other)
        ),
    }

    let android = &manifest.platforms.android;
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| default_out_dir(project_dir, "android"));

    // AGP 8 does not fall back to the debug keystore for release builds, so without a keystore the
    // APK/AAB would be unsigned (and uninstallable). Fail instead of shipping an unsigned release.
    require_signing_keystore(args.profile, android)?;

    // Check toolchain.
    let sdk = toolchain::find_sdk()?;
    toolchain::check_sdk_platform(&sdk, android.target_sdk)?;
    let ndk = toolchain::find_ndk(&sdk).with_context(|| {
        format!(
            "Android NDK not found (ANDROID_NDK_HOME unset, nothing in {}). Install it with \
             `sdkmanager \"ndk;28.0.13004108\"` or set ANDROID_NDK_HOME",
            sdk.join("ndk").display()
        )
    })?;
    toolchain::check_java()?;
    let gradle = toolchain::find_gradle()?;
    let workspace_root = cargo_workspace_root(project_dir)?;
    toolchain::check_rust_targets(&workspace_root, &android.abis)?;
    tracing::info!(sdk = %sdk.display(), ndk = %ndk.display(), gradle = %gradle.display(), "Android toolchain");

    let gradle_dir = gradle_dir(project_dir);
    for generated in [GENERATED_SOURCE_DIR, ASSET_PACKS_DIR] {
        let generated = gradle_dir.join(generated);
        if generated.exists() {
            std::fs::remove_dir_all(&generated)
                .with_context(|| format!("removing stale {}", generated.display()))?;
        }
    }
    let jni_libs_dir = gradle_dir.join(JNI_LIBS_DIR);
    std::fs::create_dir_all(&jni_libs_dir).with_context(|| format!("creating {}", jni_libs_dir.display()))?;

    // 1. Native code.
    let mut ndk_args = cargo_ndk_args(android, &jni_libs_dir, args.profile);
    if args.profile == Profile::Shipping {
        for value in shipping_profile_config(&workspace_root) {
            ndk_args.push("--config".into());
            ndk_args.push(value.into());
        }
    }
    process::run(
        Command::new("cargo")
            .args(ndk_args)
            .current_dir(&workspace_root)
            .env("ANDROID_HOME", &sdk)
            .env("ANDROID_NDK_HOME", &ndk),
        "Install cargo-ndk with `cargo install cargo-ndk` and the Android targets with \
         `rustup target add aarch64-linux-android x86_64-linux-android`",
    )
    .context("building the Android native library")?;
    for abi in &android.abis {
        let lib = jni_libs_dir.join(abi).join(format!("lib{NATIVE_LIB_NAME}.so"));
        if !lib.is_file() {
            bail!(
                "{} was not produced. The package `{}` must have `[lib] name = \"{NATIVE_LIB_NAME}\"` and \
                 `crate-type = [\"cdylib\"]` (set [platforms.android] cargo_package in Regia.toml if \
                 the Android crate has another name)",
                lib.display(),
                android.cargo_package
            );
        }
    }

    // 1b. Symbolication artifact (ADR-2631): AGP strips the libraries while it packages them, so a
    // shipping build keeps the unstripped copies of every ABI now, before Gradle runs.
    if args.profile == Profile::Shipping {
        let symbols = crate::android::symbols::collect_symbols(
            &jni_libs_dir,
            &out_dir,
            &android.abis,
            NATIVE_LIB_NAME,
            &manifest.project.version,
            manifest.project.build_number,
        )?;
        eprintln!("symbols: {}", symbols.display());
    }

    // 2. Gradle project, with one asset-pack module per fast-follow / on-demand chunk of a chunked
    // cook (specs/40 § 2.6). A build profile ships one closure pak, so it has no packs.
    let resolved = build_profile::resolve(manifest, &args.profile_name)?;
    let pak_dir = project_dir.join("build").join("paks").join("android");
    let packs = if resolved.spec.is_none() {
        android_asset_packs(&pak_dir)?
    } else {
        Vec::new()
    };
    if !packs.is_empty() && fmt == Format::Apk {
        eprintln!(
            "warning: {} store asset pack(s) ship only in an app bundle (--format aab); this APK carries \
             the install chunks only",
            packs.len()
        );
    }
    let files = gradle_project::render_project(
        manifest,
        &Toolchain {
            sdk_dir: sdk.clone(),
            ndk: Some(Ndk {
                version: toolchain::ndk_version(&ndk),
                dir: ndk,
            }),
        },
        &packs,
    )?;
    gradle_project::write_files(&gradle_dir, &files)?;

    // 3. Pak files into assets.
    // 3a. Raw (non-cookable) assets + notices into the APK assets root, so
    // `assets://about.txt` resolves from the APK (the VFS APK mount). A build profile ships only its
    // closure; shipping builds run ship_check over the staged content.
    let apk_assets = gradle_dir.join(APK_ASSETS_DIR);
    stage_content(
        project_dir,
        "android",
        &resolved,
        &apk_assets,
        &apk_assets.join("content"),
        &apk_assets,
        || stage_raw_assets(project_dir, &apk_assets, args.profile, "android"),
    )?;
    let sbom_targets: Vec<String> = android
        .abis
        .iter()
        .filter_map(|abi| toolchain::abi_triple(abi))
        .map(str::to_owned)
        .collect();
    copy_notices(
        &gradle_dir.join(APK_ASSETS_DIR),
        project_dir,
        &sbom_targets,
        args.no_engine_credit,
    )?;
    // The package's build facts, with the platform's render profile, beside the content (ADR-2631).
    let mut build_info_extra = serde_json::Map::new();
    insert_render_profile(&mut build_info_extra, manifest, "android");
    write_build_info(&apk_assets, project_dir, manifest, build_info_extra)?;
    // 3a'. Pack chunks into their asset-pack modules (never into the base module).
    stage_asset_packs(&pak_dir, &gradle_dir, &packs)?;

    // 3b. Icons if configured.
    copy_android_icons(project_dir, &gradle_dir, &manifest.platforms.android)?;

    // Inspect the uncompressed assets and native libraries before Gradle archives them.
    check_package_hygiene(project_dir, &apk_assets, args.profile)?;
    check_package_hygiene(project_dir, &jni_libs_dir, args.profile)?;
    if fmt == Format::Aab {
        for pack in &packs {
            let asset = gradle_dir.join(pack.asset_path());
            if let Some(parent) = asset.parent() {
                check_package_hygiene(project_dir, parent, args.profile)?;
            }
        }
    }

    // 4. Build APK or AAB.
    let slug = slugify(&manifest.project.id)
        .unwrap_or_else(|| slugify(&manifest.project.name).unwrap_or_else(|| "app".to_string()));

    if fmt == Format::Aab {
        process::run(
            Command::new(&gradle)
                .arg(if args.profile == Profile::Shipping {
                    "bundleRelease"
                } else {
                    "bundleDebug"
                })
                .current_dir(&gradle_dir)
                .env("ANDROID_HOME", &sdk),
            "Set REGIA_GRADLE to a Gradle launcher",
        )
        .context("Gradle build of the AAB failed")?;

        let bundle_dir = if args.profile == Profile::Shipping {
            "release"
        } else {
            "debug"
        };
        let built = gradle_dir
            .join("app/build/outputs/bundle")
            .join(bundle_dir)
            .join(format!("app-{bundle_dir}.aab"));
        if !built.is_file() {
            bail!("Gradle succeeded but AAB not found at {}", built.display());
        }
        std::fs::create_dir_all(&out_dir)?;
        let aab = out_dir.join(format!("{slug}.aab"));
        std::fs::copy(&built, &aab)?;
        // The signed bundle is read in place; its receipt is a sidecar.
        final_artifact_hook(
            project_dir,
            &aab,
            args.profile,
            &["base/assets/".to_owned(), "base/lib/".to_owned()],
        )?;
        tracing::info!(aab = %aab.display(), "AAB ready");
        print_size_report(&aab, &manifest.platforms.android.abis);
        outln!("{}", aab.display());
        return Ok(aab);
    }

    // APK.
    let assemble_task = if args.profile == Profile::Shipping {
        "assembleRelease"
    } else {
        "assembleDebug"
    };
    process::run(
        Command::new(&gradle)
            .arg(assemble_task)
            .current_dir(&gradle_dir)
            .env("ANDROID_HOME", &sdk),
        "Set REGIA_GRADLE to a Gradle launcher",
    )
    .context("Gradle build of the APK failed")?;

    let apk_dir = if args.profile == Profile::Shipping {
        "release"
    } else {
        "debug"
    };
    let built = gradle_dir
        .join("app/build/outputs/apk")
        .join(apk_dir)
        .join(format!("app-{apk_dir}.apk"));
    if !built.is_file() {
        bail!("Gradle succeeded but {} does not exist", built.display());
    }
    std::fs::create_dir_all(&out_dir)?;
    let apk = out_dir.join(format!("{slug}-{apk_dir}.apk"));
    std::fs::copy(&built, &apk)?;
    // The signed APK is read in place; its receipt is a sidecar.
    final_artifact_hook(
        project_dir,
        &apk,
        args.profile,
        &["assets/".to_owned(), "lib/".to_owned()],
    )?;
    tracing::info!(apk = %apk.display(), "APK ready");
    print_size_report(&apk, &manifest.platforms.android.abis);
    outln!("{}", apk.display());
    Ok(apk)
}

/// Prints the built archive's size report on stderr (`regia apk-size`, ADR-527); stdout keeps
/// only the archive path. A report that cannot be read only warns: the package is built.
fn print_size_report(archive: &Path, abis: &[String]) {
    match super::apk_size::report_for_file(archive, abis, 10, None) {
        Ok(report) => eprint!("{}", super::apk_size::render_text(&report)),
        Err(err) => tracing::warn!("size report for {} skipped: {err:#}", archive.display()),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn platform_dir_name(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "linux",
        Platform::Windows => "windows",
        Platform::Android => "android",
    }
}

fn format_name(format: Format) -> String {
    format
        .to_possible_value()
        .map_or_else(|| format!("{format:?}"), |v| v.get_name().to_owned())
}

/// Release builds must be signed with a configured keystore: AGP 8 does **not** fall back to the
/// debug keystore for `assembleRelease`/`bundleRelease`, so a missing keystore produces an unsigned
/// APK/AAB that no device installs. Dev builds use the AGP debug keystore and need no project key.
fn require_signing_keystore(profile: Profile, android: &AndroidPlatform) -> Result<()> {
    if profile == Profile::Shipping && !android.has_signing_keystore() {
        bail!(
            "release signing requested but no [platforms.android] keystore is configured; \
             AGP 8 leaves release builds unsigned (it does not fall back to the debug keystore). \
             Set `keystore` in Regia.toml (plus the password/alias env vars), or build with \
             `--profile dev` for a debug-signed APK"
        );
    }
    Ok(())
}

/// The directory name a desktop package is staged under (`dist/<platform>/<slug>`): the slugged
/// app id, else the slugged name. `regia steam` addresses depots by the same name.
pub fn staging_slug(manifest: &Manifest) -> String {
    let name = &manifest.project.name;
    slugify(&manifest.project.id).unwrap_or_else(|| slugify(name).unwrap_or_else(|| name.clone()))
}

/// Turn a project id or name into a filesystem-safe slug with no spaces.
///
/// E.g. `com.example.My Game` → `com-example-my-game`.
fn slugify(raw: &str) -> Option<String> {
    let slug: String = raw
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            ' ' | '\t' | '/' | '\\' | ':' => '-',
            _ => '-',
        })
        .collect();
    let compact: String = slug
        .chars()
        .fold((String::new(), false), |(mut acc, prev_dash), c| {
            let is_dash = c == '-';
            if is_dash && prev_dash {
                (acc, true)
            } else {
                acc.push(c);
                (acc, is_dash)
            }
        })
        .0;
    let trimmed = compact.trim_matches('-');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_lowercase())
    }
}

/// Cook the project for the target platform when paks are missing or stale.
///
/// Cooks the project for the target platform when the paks are missing, empty, or stale, then
/// (re)packs them into `build/paks/<platform>/`. A fresh `base.fpak` (valid, non-empty, with every
/// cookable source's blake3 content hash matching the asset database, and not older than that
/// database) skips the cook. After a successful pack the database rows for deleted sources are
/// pruned, so a later package run sees the pak as fresh instead of recooking forever.
///
/// The paks also need the pack stamp (`PACK_STAMP_FILE`, ADR-7413): it is removed before a
/// cook, written only after the whole pack finished, and names the project manifest (chunk rules,
/// startup level, content host and keys, the version in the build id), the asset database and the
/// pack options (`PackOptions`, ADR-7612) the pack was made from. Paks packed with other options
/// (`regia cook --quality fast` or `--gpu-meshes`) are packed again.
fn ensure_paks(project_dir: &std::path::Path, _manifest: &Manifest, args: &PackageArgs) -> Result<()> {
    let platform = build_profile::platform_name(args.platform);
    let pak_dir = project_dir.join("build").join("paks").join(platform);
    // Source diagnostics before any cook or a fresh-pak short circuit (ADR-8390).
    let _lint = super::assets_validate::enforce_publish_lint(project_dir, true)?;
    let base_pak = pak_dir.join("base.fpak");

    // `[provenance] require_review_for_shipping` (specs/23 § 5, ADR-2346): a package refuses
    // unreviewed agent- and generator-made assets, fresh paks or not.
    super::review_gate::check(project_dir)?;

    let options = PackOptions::for_package(project_dir, platform);
    if base_pak_is_fresh(&base_pak, project_dir)
        && pack_stamp_matches_with(project_dir, &pak_dir, options)
        && super::plugin::cooked_extensions_fresh(project_dir, platform, &pak_dir)
    {
        super::content::check_project_integrity(
            project_dir,
            &regia_asset::cooked_cache_dir(project_dir, platform),
        )?;
        tracing::info!(pak=%base_pak.display(), "paks exist and are up to date, skipping cook");
        return Ok(());
    }
    // A run that stops anywhere from here on leaves no stamp, so the next one packs again.
    remove_pack_stamp(&pak_dir)?;

    tracing::info!(platform, "cooking project before packaging");
    let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
        project_root: project_dir.to_path_buf(),
        platform: platform.to_string(),
        incremental: true,
    })?;
    super::cook::report_cook_outcome(&report)?;
    super::plugin::cook_extensions(project_dir, platform, true)?;

    // Validate the cooked registry (spec 32 § 4.5): a package step means `Fail` unless the
    // config names `cook` itself (ADR-4908). `regia cook` keeps the `Warn` default.
    super::assets_validate::cook_validation_for(project_dir, platform, true)?;

    // Drop database rows for sources that no longer exist (or became a glTF sidecar) and save the
    // pruned database *before* packing. This does two things: a deleted source stops forcing every
    // later package to recook, and a failed pack leaves `base.fpak` older than the freshly-saved
    // database, which `base_pak_is_fresh` treats as stale (so the next run repacks rather than
    // shipping the leftover pak).
    prune_asset_db(project_dir)?;

    // Generate pak files.
    let cache_dir = regia_asset::cooked_cache_dir(project_dir, platform);
    // One pak per `[cook.chunks]` rule (only `base.fpak` without rules) plus the content manifest
    // (specs/40).
    match super::content::pack_project_chunks(
        project_dir,
        &cache_dir,
        &pak_dir,
        platform,
        options.compress,
        options.gpu_meshes,
    ) {
        Ok(paks) => {
            eprintln!("pak: wrote {} pak file(s) to {}", paks.len(), pak_dir.display());
        }
        Err(e) => {
            return Err(e).context("pak generation failed");
        }
    }
    super::plugin::write_cooked_record(project_dir, platform, &pak_dir)?;
    write_pack_stamp_with(project_dir, &pak_dir, options)?;

    Ok(())
}

/// The record of a finished pack in `build/paks/<platform>/` (ADR-7413), written by `ensure_paks`
/// and by `regia cook`. The package stages paks by name or by extension, so it never ships.
const PACK_STAMP_FILE: &str = "package-pack.stamp";

/// The pack options that change the bytes of the paks (ADR-7612), so the pack stamp records them
/// and the package takes only a pack that [`Self::serves`] it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PackOptions {
    /// Entries are compressed (`regia package`, `regia cook --quality final`).
    pub(crate) compress: bool,
    /// Meshes are stored GPU-ready (`regia cook --gpu-meshes` or `--ship`, or
    /// `REGIA_PAK_GPU_MESHES` on a desktop platform).
    pub(crate) gpu_meshes: bool,
}

impl PackOptions {
    /// What `regia package` packs `platform` with: always compressed, meshes GPU-ready when
    /// `regia_asset_pipeline::gpu_meshes_for` says so for the platform's cooked cache.
    pub(crate) fn for_package(project_dir: &Path, platform: &str) -> Self {
        Self {
            compress: true,
            gpu_meshes: regia_asset_pipeline::gpu_meshes_for(&regia_asset::cooked_cache_dir(
                project_dir,
                platform,
            )),
        }
    }

    /// Whether paks packed with `self` serve a package that packs with `wanted` (ADR-7612): the
    /// compression must match, so the uncompressed paks of `regia cook --quality fast` are packed
    /// again; GPU-ready meshes a `regia cook --ship` or `--gpu-meshes` chose are kept even when the
    /// package itself would keep cooked meshes (the shipping flow of the cooked-pack guide), while a
    /// package that wants GPU-ready meshes packs again over cooked ones.
    pub(crate) fn serves(self, wanted: PackOptions) -> bool {
        self.compress == wanted.compress && (self.gpu_meshes || !wanted.gpu_meshes)
    }
}

/// The platform a pak directory (`build/paks/<platform>`) holds.
#[cfg(test)]
fn pak_dir_platform(pak_dir: &Path) -> &str {
    pak_dir.file_name().and_then(|n| n.to_str()).unwrap_or_default()
}

/// What the pack stamp holds for the project as it is now, packed with `options`: the blake3 of
/// the manifest the project loads (`Regia.toml`, or the legacy manifest of a project that has only
/// that one, ADR-1388; `absent` without either), of the asset database, and the pack options.
/// `None` when a file cannot be read, so freshness cannot be proven. Version 2 (ADR-7612) added
/// the manifest's name and the options, so a version 1 stamp never matches.
fn pack_stamp(project_dir: &Path, options: PackOptions) -> Option<String> {
    let manifest = match regia_core::brand::project_manifest(project_dir) {
        Some(path) => {
            let bytes = std::fs::read(&path).ok()?;
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            format!("{name} {}", blake3::hash(&bytes).to_hex())
        }
        None => "absent".to_owned(),
    };
    let db = std::fs::read(regia_asset::asset_db_path(project_dir)).ok()?;
    Some(format!(
        "regia-pack-stamp 2\nmanifest {manifest}\nasset_db {}\ncompress {}\ngpu_meshes {}\n",
        blake3::hash(&db).to_hex(),
        u8::from(options.compress),
        u8::from(options.gpu_meshes),
    ))
}

/// Whether the last pack into `pak_dir` finished, was made from the current project manifest and
/// asset database, and with options that [`PackOptions::serves`] a package packing with `wanted`:
/// `[cook.chunks]` rules, the startup level and `[content]` live in the manifest, not under
/// `assets/`, a pack stopped after `base.fpak` leaves no stamp, and paks packed with other options
/// have other bytes.
fn pack_stamp_matches_with(project_dir: &Path, pak_dir: &Path, wanted: PackOptions) -> bool {
    let Ok(text) = std::fs::read_to_string(pak_dir.join(PACK_STAMP_FILE)) else {
        return false;
    };
    [false, true]
        .into_iter()
        .flat_map(|compress| [false, true].map(|gpu_meshes| PackOptions { compress, gpu_meshes }))
        .filter(|options| options.serves(wanted))
        .any(|options| pack_stamp(project_dir, options).is_some_and(|expected| text == expected))
}

/// [`pack_stamp_matches_with`] for the options `regia package` packs `pak_dir`'s platform with.
#[cfg(test)]
fn pack_stamp_matches(project_dir: &Path, pak_dir: &Path) -> bool {
    let options = PackOptions::for_package(project_dir, pak_dir_platform(pak_dir));
    pack_stamp_matches_with(project_dir, pak_dir, options)
}

/// Removes the pack stamp before a cook and pack (a missing one is fine).
pub(crate) fn remove_pack_stamp(pak_dir: &Path) -> Result<()> {
    let path = pak_dir.join(PACK_STAMP_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// [`write_pack_stamp_with`] for a pack made with the options `regia package` uses for
/// `pak_dir`'s platform (test fixtures that pack the way the package does).
#[cfg(test)]
pub(crate) fn write_pack_stamp(project_dir: &Path, pak_dir: &Path) -> Result<()> {
    let options = PackOptions::for_package(project_dir, pak_dir_platform(pak_dir));
    write_pack_stamp_with(project_dir, pak_dir, options)
}

/// Writes the pack stamp of a pack made with `options` (temp file and rename) once every pak,
/// record and the extension record of the pack are written.
pub(crate) fn write_pack_stamp_with(project_dir: &Path, pak_dir: &Path, options: PackOptions) -> Result<()> {
    let stamp = pack_stamp(project_dir, options)
        .context("reading the project manifest and the asset database for the pack stamp")?;
    let path = pak_dir.join(PACK_STAMP_FILE);
    let tmp = pak_dir.join(format!("{PACK_STAMP_FILE}.tmp{}", std::process::id()));
    std::fs::write(&tmp, stamp).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

/// Drop asset-database rows whose source no longer exists (or is now a glTF sidecar) and save the
/// pruned database.
///
/// `cook_project` inserts a row for every cookable source it cooks but never removes rows, so a
/// deleted source leaves an orphan row behind. `base_pak_is_fresh` treats any recorded-but-absent
/// source as stale, which would otherwise recook every later package. Pruning after a cook (and
/// before packing) keeps the database to exactly the sources the just-written pak was built from.
fn prune_asset_db(project_dir: &Path) -> Result<()> {
    let mut db = regia_asset_pipeline::AssetDatabase::open(project_dir)?;
    let assets = project_dir.join("assets");
    // Skip glTF sidecars too: once a png/jpg is referenced by a `.gltf` it is cooked through that
    // glTF and must not keep a standalone row.
    let sources = regia_asset_pipeline::AssetDatabase::discover_sources(&assets);
    let sidecars = regia_asset_pipeline::gltf_sidecars(&sources);
    db.entries.retain(|path, _| {
        // Extension content has no file under `assets/`; `regia cook` keeps those rows current.
        if path.starts_with("ext://") {
            return true;
        }
        let rel = path.strip_prefix("assets/").unwrap_or(path.as_str());
        let file = assets.join(rel);
        file.is_file() && !sidecars.contains(&file)
    });
    db.save(project_dir)?;
    Ok(())
}

/// Whether `base.fpak` is fresh enough to skip cooking: it must exist, parse as a valid pak with at
/// least one entry, and every cookable source under `<project>/assets/` must match the blake3 source
/// hash the asset database recorded when the pak was cooked (`DbEntry.source_hash`). A missing,
/// unreadable, empty, or content-stale pak returns `false` so `ensure_paks` recooks.
///
/// Freshness is a *content* check, not an mtime check: a source whose bytes changed — or one that was
/// added or removed — since the cook is stale even when its mtime is older than the pak. The previous
/// check compared only the pak's mtime against the newest source, which shipped a leftover pak whose
/// content no longer matched `assets/`.
///
/// The database is also ordered against the pak: `ensure_paks` saves the database before packing, so a
/// pak older than the database predates the last cook and cannot reflect the current sources (a failed
/// pack leaves exactly this state). glTF sidecars (buffers/images referenced by URI) are skipped because
/// the cook records a database row only for the `.gltf`, never for its sidecars.
///
/// Desktop and Android terrains are checked through their layer file's `source_hash` plus the
/// `settings_hash` over their inputs (heightfield, macro/layer textures, `heightfield.json`), which
/// have no database row of their own; the platform is read from the pak directory name.
fn base_pak_is_fresh(base_pak: &Path, project_dir: &Path) -> bool {
    // Missing or unreadable (corrupt/truncated) → recook.
    let Ok(reader) = regia_asset::PakReader::open(base_pak) else {
        return false;
    };
    // An empty pak (zero entries) carries no content → recook.
    if reader.toc.entries.is_empty() {
        return false;
    }

    // The asset database records the blake3 of each cookable source's bytes at the last cook
    // (`DbEntry.source_hash`), keyed by its `assets/…` path. `ensure_paks` cooks before packing, so
    // the database records exactly the bytes the pak was built from. A missing database file (or one
    // that cannot be parsed) means freshness cannot be proven → recook.
    let db_path = regia_asset::asset_db_path(project_dir);
    if !db_path.is_file() {
        return false;
    }
    let Ok(db) = regia_asset_pipeline::AssetDatabase::open(project_dir) else {
        return false;
    };

    // A pak older than the database predates the last cook: `ensure_paks` saves the database before
    // packing, so a failed pack leaves the pak older and must be repacked. Unreadable mtimes mean
    // freshness cannot be proven → recook.
    let Ok(db_mtime) = std::fs::metadata(&db_path).and_then(|m| m.modified()) else {
        return false;
    };
    let Ok(pak_mtime) = std::fs::metadata(base_pak).and_then(|m| m.modified()) else {
        return false;
    };
    if pak_mtime < db_mtime {
        return false;
    }

    // An `assets` path that is not a directory means freshness cannot be proven → recook.
    let assets = project_dir.join("assets");
    if !assets.is_dir() {
        return false;
    }

    // Platform is the pak directory name (`build/paks/<platform>/base.fpak`). Desktop and Android
    // both cook terrain inputs (heightfield, macro/layer textures, heightfield.json) through the
    // tiled path and leave them no database row of their own. Freshness must hash exactly the
    // sources the cook records.
    let platform = base_pak
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let sources = regia_asset_pipeline::AssetDatabase::discover_sources(&assets);
    // glTF buffers/images referenced by URI are cooked through their `.gltf` and have no database row
    // of their own; skip them (the same skip list `cook_project` uses).
    let sidecars = regia_asset_pipeline::gltf_sidecars(&sources);
    // Terrain layer files cook through the tiled path on both platforms and are hashed separately below;
    // their inputs have no database row of their own.
    let terrain_layers: std::collections::BTreeSet<PathBuf> =
        regia_asset_pipeline::discover_terrains(&sources)
            .into_iter()
            .map(|t| t.source)
            .collect();
    let terrain_inputs = regia_asset_pipeline::terrain_input_files_for(&sources, &platform);

    // Walk `assets/` and hash every generic cookable source. Any error walking the tree (or reading a
    // source) means freshness cannot be proven → recook.
    let mut present: std::collections::BTreeMap<String, [u8; 32]> = std::collections::BTreeMap::new();
    for entry in walkdir::WalkDir::new(&assets).into_iter() {
        let Ok(entry) = entry else {
            return false;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(&assets) else {
            continue;
        };
        if sidecars.contains(entry.path()) {
            continue;
        }
        // Terrain inputs and terrain layers are handled separately, not hashed generically.
        if terrain_inputs.contains(entry.path()) || terrain_layers.contains(entry.path()) {
            continue;
        }
        if !is_cookable_source(rel) {
            continue;
        }
        // An editor-only generator's bake is never cooked (ADR-2421, ADR-7214): no row to compare.
        if rel.extension().is_some_and(|e| e.eq_ignore_ascii_case("pgout"))
            && !regia_procgen::cook::bake_ships(entry.path())
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            return false;
        };
        present.insert(
            format!("assets/{}", regia_asset::normalise_asset_path(rel)),
            *blake3::hash(&bytes).as_bytes(),
        );
    }

    // Terrain layers: a cookable terrain records its layer-file bytes as `source_hash` and the
    // hash of its discovered inputs as `settings_hash`. An empty terrain (no heightfield and no
    // layer texture) is left uncooked with no row and must not force a recook. Android uses the
    // same rows; the tile bytes (BCn or ASTC) live in the platform cache, not in this hash.
    let mut terrain_layer_paths: Vec<(String, PathBuf)> = Vec::new();
    for layer in &terrain_layers {
        let Ok(rel) = layer.strip_prefix(&assets) else {
            return false;
        };
        let rel_path = format!("assets/{}", regia_asset::normalise_asset_path(rel));
        match regia_asset_pipeline::terrain_is_cookable(layer) {
            Ok(true) => {
                let Ok(bytes) = std::fs::read(layer) else {
                    return false;
                };
                present.insert(rel_path.clone(), *blake3::hash(&bytes).as_bytes());
                terrain_layer_paths.push((rel_path, layer.clone()));
            }
            Ok(false) => {
                // A row still present means the terrain became empty since its last cook.
                if db.entries.contains_key(&rel_path) {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }

    // A cookable source added since the cook (no database row), or whose bytes changed → recook.
    for (path, hash) in &present {
        match db.entries.get(path) {
            Some(row) if row.source_hash == *hash => {}
            _ => return false,
        }
    }

    // A cookable source removed since the cook (still recorded, now missing) → recook. Rows this
    // platform does not record are ignored: the database is shared across platforms, so a row left
    // by another platform's cook (a terrain input consumed by the tiled path, which has no generic
    // row of its own) still has its source file on disk and must not pin this pak stale.
    for path in db.entries.keys() {
        // Extension content is `cooked_extensions_fresh`'s to judge.
        if present.contains_key(path) || path.starts_with("ext://") {
            continue;
        }
        let rel = path.strip_prefix("assets/").unwrap_or(path);
        let file = assets.join(rel);
        if !file.is_file() || sidecars.contains(&file) {
            // The source is genuinely gone (deleted, or now a glTF sidecar) → stale.
            return false;
        }
        // The source still exists but is excluded from this platform's cook: not an orphan.
    }

    // Terrains: verify the settings hash recorded at cook time against the current inputs
    // (the loop above guarantees each terrain layer path has a row with a matching source hash).
    for (rel_path, layer) in &terrain_layer_paths {
        let Some(row) = db.entries.get(rel_path) else {
            return false;
        };
        let Ok(settings_hash) = regia_asset_pipeline::terrain_settings_hash(layer) else {
            return false;
        };
        if row.settings_hash != settings_hash {
            return false;
        }
    }

    true
}

/// The cargo package to build and the name of the binary it produces.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CargoTarget {
    /// `-p` argument: the `[package] name`.
    pub(crate) package: String,
    /// Binary file stem: the first `[[bin]] name`, else the package name.
    pub(crate) bin: String,
}

/// Parse a `Cargo.toml` that has a `[package]` table; `None` for a virtual workspace.
fn parse_cargo_target(text: &str) -> Option<CargoTarget> {
    let doc: toml::Value = toml::from_str(text).ok()?;
    let package = doc.get("package")?.get("name")?.as_str()?.to_string();
    let bin = doc
        .get("bin")
        .and_then(toml::Value::as_array)
        .and_then(|bins| bins.first())
        .and_then(|b| b.get("name"))
        .and_then(toml::Value::as_str)
        .map_or_else(|| package.clone(), str::to_string);
    Some(CargoTarget { package, bin })
}

/// The package and binary to ship for the project in `project_dir`: the root `Cargo.toml` when it
/// has a `[package]`, else `launcher/Cargo.toml` (what `regia new` generates, the same crate
/// `regia run` builds).
pub(crate) fn cargo_package_name(project_dir: &Path) -> Result<CargoTarget> {
    let mut tried = Vec::new();
    for path in [
        project_dir.join("Cargo.toml"),
        project_dir.join("launcher").join("Cargo.toml"),
    ] {
        let Ok(content) = std::fs::read_to_string(&path) else {
            tried.push(path.display().to_string());
            continue;
        };
        if let Some(target) = parse_cargo_target(&content) {
            return Ok(target);
        }
        tried.push(path.display().to_string());
    }
    bail!(
        "no [package] name in {}: a project needs a root package or a launcher/ package",
        tried.join(" or ")
    )
}

/// The cargo call that builds one package: where it runs, its arguments after `cargo`, and the
/// variables it adds to the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CargoBuild {
    /// The cargo workspace of the project, the call's working directory.
    pub(crate) workspace: PathBuf,
    /// The arguments after `cargo`.
    pub(crate) args: Vec<String>,
    /// Variables added to the inherited environment.
    pub(crate) env: Vec<(&'static str, String)>,
}

/// The cargo call [`build_cargo`] makes for the target platform.
///
/// Shipping builds remap the build machine's paths ([`remap_config`]), pass the engine's shipping
/// profile when the project's workspace defines none ([`shipping_profile_config`]), and bake the
/// build identity into the binary (`REGIA_GIT_HASH`, `REGIA_BUILD_VERSION`, `REGIA_BUILD_PROFILE`,
/// read by `regia_platform::crash::BuildInfo`). Dev builds set neither, so they keep sharing the
/// incremental cache with `cargo build`.
///
/// A build step that runs before `regia package` (a CI job that builds while the tests compile;
/// `tools/gaterig/package-prebuild.sh` for the merge gate, ADR-5719) has to make this same call:
/// any other flag or identity value is another build to cargo, and the package rebuilds every crate.
pub(crate) fn cargo_build_call(
    project_dir: &Path,
    package: &str,
    resolved: &ResolvedProfile,
    target: Option<&str>,
) -> Result<CargoBuild> {
    let profile = resolved.cargo;
    let workspace = cargo_workspace_root(project_dir)?;
    let mut config = Vec::new();
    let mut env = Vec::new();
    if profile == Profile::Shipping {
        let triple = match target {
            Some(t) => Some(cargo_target_dir_name(t).to_string()),
            None => host_triple(),
        };
        let target_dir = match regia_core::brand::env_var_os("CARGO_TARGET_DIR").filter(|d| !d.is_empty()) {
            Some(dir) => workspace.join(dir),
            None => workspace.join("target"),
        };
        match triple {
            Some(triple) => config.push(remap_config(&triple, &workspace, &target_dir)),
            None => tracing::warn!(
                "cannot determine the host triple (`rustc -vV`); shipping paths are not remapped"
            ),
        }
        config.extend(shipping_profile_config(&workspace));
        let manifest = Manifest::load(project_dir)?;
        env.push(("REGIA_GIT_HASH", git_revision(project_dir)));
        env.push(("REGIA_BUILD_VERSION", manifest.project.version.clone()));
        env.push(("REGIA_BUILD_PROFILE", "shipping".to_string()));
    }
    let mut args = cargo_build_args(package, profile, target, build_tool(target), &config);
    args.extend(build_profile::cargo_extra_args(resolved));
    Ok(CargoBuild { workspace, args, env })
}

/// Build the cargo project for the target platform (the call of [`cargo_build_call`]).
pub(crate) fn build_cargo(
    project_dir: &Path,
    package: &str,
    resolved: &ResolvedProfile,
    target: Option<&str>,
) -> Result<()> {
    let tool = build_tool(target);
    if tool == BuildTool::Xwin && !cargo_xwin_installed() {
        bail!(
            "cross-compiling {} needs cargo-xwin on PATH (specs/21 § 4: Linux→Windows is cargo-xwin, \
             which supplies the MSVC CRT and SDK). Install it with `cargo install cargo-xwin` and re-run \
             `regia package --platform windows`",
            target.unwrap_or(WINDOWS_MSVC_TARGET)
        );
    }
    if tool == BuildTool::Zigbuild && !(on_path("cargo-zigbuild") && on_path("zig")) {
        bail!(
            "building against glibc {} (Steam Linux Runtime 3.0) needs cargo-zigbuild and zig on PATH: \
             `cargo install --locked cargo-zigbuild` and zig 0.14+ (https://ziglang.org/download/). \
             `--glibc host` builds against this machine's glibc instead (not runnable in the Steam runtime)",
            target
                .and_then(|t| t.rsplit_once("-gnu."))
                .map_or("?", |(_, v)| v)
        );
    }
    let call = cargo_build_call(project_dir, package, resolved, target)?;
    let hint = match tool {
        BuildTool::Xwin => "Install cargo-xwin (`cargo install cargo-xwin`) and Rust from https://rustup.rs",
        BuildTool::Zigbuild => "Install cargo-zigbuild (`cargo install --locked cargo-zigbuild`) and zig",
        BuildTool::Cargo => "Install Rust from https://rustup.rs",
    };
    let mut cmd = Command::new("cargo");
    cmd.envs(call.env.iter().map(|(name, value)| (*name, value.as_str())));
    process::run(cmd.args(&call.args).current_dir(&call.workspace), hint).context("building the binary")?;
    Ok(())
}

/// The host side of a `--container` build: the call [`cargo_build_call`] would make for the host
/// (no `--target`: the container's own glibc is the floor), with shipping paths remapped from the
/// container's per-profile target directory and cargo home.
fn container_host_call(
    project_dir: &Path,
    package: &str,
    bin: &str,
    resolved: &ResolvedProfile,
    profile: package_container::ContainerProfile,
) -> Result<package_container::HostCall> {
    let workspace = cargo_workspace_root(project_dir)?;
    let target_dir = match regia_core::brand::env_var_os("CARGO_TARGET_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => workspace.join(dir),
        None => workspace.join("target"),
    };
    let dirs = package_container::ProfileDirs::new(&target_dir, profile);
    let mut config = Vec::new();
    let mut identity = Vec::new();
    if resolved.cargo == Profile::Shipping {
        let triple = format!("{}-unknown-linux-gnu", std::env::consts::ARCH);
        config.push(remap_config_paths(
            &triple,
            None,
            Some(&dirs.cargo_home),
            &workspace,
            &dirs.target,
        ));
        config.extend(shipping_profile_config(&workspace));
        let manifest = Manifest::load(project_dir)?;
        identity.push(("REGIA_GIT_HASH", git_revision(project_dir)));
        identity.push(("REGIA_BUILD_VERSION", manifest.project.version.clone()));
        identity.push(("REGIA_BUILD_PROFILE", "shipping".to_string()));
    }
    let mut args = cargo_build_args(package, resolved.cargo, None, BuildTool::Cargo, &config);
    args.extend(build_profile::cargo_extra_args(resolved));
    Ok(package_container::HostCall {
        workspace,
        target_dir,
        args,
        identity,
        profile_dir: match resolved.cargo {
            Profile::Dev => "debug",
            Profile::Shipping => "shipping",
        },
        bin: bin.to_string(),
    })
}

pub(crate) fn find_binary(
    project_dir: &Path,
    package: &str,
    profile: Profile,
    is_windows: bool,
    target_triple: Option<&str>,
) -> Result<PathBuf> {
    let workspace = cargo_workspace_root(project_dir)?;
    let profile_dir = match profile {
        Profile::Dev => "debug",
        Profile::Shipping => "shipping",
    };

    let exe_name = if is_windows {
        format!("{package}.exe")
    } else {
        package.to_string()
    };
    let candidates = binary_candidates(
        &workspace,
        project_dir,
        regia_core::brand::env_var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .as_deref(),
        target_triple,
        profile_dir,
        &exe_name,
    );
    if let Some(path) = candidates.iter().find(|path| path.is_file()) {
        return Ok(path.clone());
    }
    let tried: Vec<String> = candidates.iter().map(|path| path.display().to_string()).collect();
    bail!(
        "binary `{exe_name}` not found (tried {}). Build it first with `cargo build --profile {profile_dir} -p {package}`",
        tried.join(", ")
    );
}

/// Where `cargo build` put the binary, in lookup order: `CARGO_TARGET_DIR` (the same variable
/// [`build_cargo`]'s cargo honours; a relative value is relative to the workspace, the build's
/// working directory), then `<workspace>/target`, then the project-local `target`.
///
/// When `target_triple` is set, `<dir>/<triple>/<profile>/` is searched before the host profile
/// directory. Cross builds (`cargo xwin build --target …`) write the exe only under the triple.
fn binary_candidates(
    workspace: &Path,
    project_dir: &Path,
    cargo_target_dir: Option<&Path>,
    target_triple: Option<&str>,
    profile_dir: &str,
    exe_name: &str,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = cargo_target_dir.filter(|dir| !dir.as_os_str().is_empty()) {
        dirs.push(workspace.join(dir));
    }
    dirs.push(workspace.join("target"));
    dirs.push(project_dir.join("target"));
    let mut out = Vec::new();
    for dir in dirs {
        // Cross builds land in `target/<triple>/<profile>/`, ahead of the host profile dir.
        if let Some(triple) = target_triple {
            out.push(dir.join(triple).join(profile_dir).join(exe_name));
        }
        out.push(dir.join(profile_dir).join(exe_name));
    }
    out
}

/// Source file extensions `regia cook` turns into cooked assets (which therefore belong in the pak,
/// not in the raw `assets/` directory of a package). Everything else under `assets/` is a raw file the
/// game reads through `assets://` at runtime (`about.txt`, config, non-cookable data) and must ship
/// verbatim next to the binary.
fn is_cookable_source(rel: &Path) -> bool {
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = rel
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "glb" | "gltf" | "png" | "jpg" | "jpeg" | "dds" | "tga" | "bmp" | "exr" | "hdr" | "svg" | "fgiv"
        | "flmap" | "cube" => true,
        // Procedural bakes cook into the pak (spec 62 § 8, ADR-7214): Android reads no loose files.
        "pgout" => true,
        "ron" | "csv" if regia_asset_pipeline::data_cook::is_data_table_name(name) => true,
        "ron" => name.ends_with(".material.ron") || name.ends_with(".scatter.ron"),
        _ => false,
    }
}

/// Cookable sources that ship as raw files as well: a host may read a `.smartobject.ron` itself and
/// set `SmartObjectDefinition` (the route older than `SmartObject::definition`) or read a
/// `.camera.ron` and call `CameraAssets::load_ron`, and these files are small text. Typed data tables
/// (`.table.ron`, `.table.csv`) ship raw as they always did (a host or a dedicated server may read them
/// through `assets://`), and their cooked form is in the pak for a game that ships without them
/// (ADR-2132). The ability formats (`.attrs.ron`, `.effect.ron`, `.ability.ron`) do not: they ship
/// cooked only (ADR-9850 (c), owner decision 2026-11-12, replacing ADR-9370's raw copy; see
/// `is_raw_ability_source`). Curve tables and choosers do not either: the budget of a large table is
/// the cooked load, and a raw file would win over the pak (`regia_framework::table_source`).
fn ships_raw_too(rel: &Path) -> bool {
    rel.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|n| {
            n.ends_with(".smartobject.ron")
                || n.ends_with(".camera.ron")
                || n.ends_with(".table.ron")
                || n.ends_with(".table.csv")
        })
}

/// The authored ability formats (`.attrs.ron`, `.effect.ron`, `.ability.ron`). A package never ships
/// them raw (ADR-9850 (c), owner decision 2026-11-12): only the cooked ability assets in the pak go
/// out, whatever the cook and shipping rules for other data say.
fn is_raw_ability_source(rel: &Path) -> bool {
    rel.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|n| {
            regia::framework::abilities::ABILITY_SOURCE_SUFFIXES
                .iter()
                .any(|suffix| n.ends_with(suffix))
        })
}

/// The message a package prints for each raw ability source it leaves out (ADR-9850 (c)).
fn raw_ability_stripped(rel: &str) -> String {
    format!(
        "package: not shipping raw ability source `{rel}`: packages ship cooked ability assets only \
         (the cooked form goes into the pak; ADR-9850)"
    )
}

/// Copy the project's raw (non-cookable) assets into `dest`, preserving their `assets/`-relative
/// paths. Cookable sources are skipped (they are cooked into the pak); `.meta` sidecars are skipped.
/// Returns the number of files copied.
/// Stages paks and raw assets, then runs `ship_check` (see [`build_profile::stage_content`]).
fn stage_content(
    project_dir: &Path,
    platform: &str,
    profile: &ResolvedProfile,
    root: &Path,
    content_dir: &Path,
    assets_dir: &Path,
    copy_plain_raw: impl FnOnce() -> Result<usize>,
) -> Result<()> {
    build_profile::stage_content(
        project_dir,
        platform,
        profile,
        root,
        content_dir,
        assets_dir,
        copy_plain_raw,
    )
}

/// The Linux `.desktop` file body. Newlines or other control characters in a field would start
/// extra keys (`Exec=`, `Path=`, ...), so they are rejected.
fn desktop_entry(name: &str, bin_name: &str) -> Result<String> {
    for (field, value) in [("[project] name", name), ("binary name", bin_name)] {
        if value.chars().any(char::is_control) {
            bail!(
                "{field} must not contain newlines or control characters (it is written into the .desktop file)"
            );
        }
    }
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name}\n\
         Exec={bin_name}\n\
         Icon={name}\n\
         Categories=Game;\n"
    ))
}

#[cfg(test)]
fn copy_raw_assets(project_dir: &Path, dest: &Path) -> Result<usize> {
    stage_raw_assets(project_dir, dest, Profile::Dev, "linux")
}

/// What a package does to the levels it stages (spec 37 § 3.6, § 4.1; spec 41 § 9).
struct LevelStaging {
    /// Drop functional tests and the files only they read (shipping).
    strip_tests: bool,
    /// The project's sources by asset id.
    index: regia_framework::level_assets::LevelSourceIndex,
    /// The platform tag the package is for: picks the audio quality (`audio_cook.toml`).
    platform: String,
}

impl LevelStaging {
    /// The staging of the project's `assets` for `profile`.
    fn new(assets: &Path, profile: Profile, platform: &str) -> Self {
        Self {
            platform: platform.to_string(),
            strip_tests: profile == Profile::Shipping,
            index: regia_framework::level_assets::LevelSourceIndex::scan(assets),
        }
    }

    /// Whether the raw file `name` ships: the bound definitions travel inside the levels, and a
    /// shipping package has no use for what only functional tests read.
    fn ships(&self, name: &str) -> bool {
        !regia_framework::level_assets::is_layer_defs_name(name)
            && !(self.strip_tests && is_test_only_asset(name))
    }

    /// The definition asset `id`, read and validated. A bad one stops the package: its members
    /// would ship unlayered.
    fn layer_defs(&self, id: regia_core::id::Guid) -> Result<Option<regia_scene::layers::LayerDefs>> {
        self.index
            .layer_defs
            .get(&id)
            .map(|path| regia_framework::level_assets::read_layer_defs(path))
            .transpose()
            .map_err(Into::into)
    }
}

/// Whether this level text needs [`stage_level`]'s work: nothing else is rewritten, so a plain
/// level ships byte for byte.
fn level_needs_staging(text: &str) -> bool {
    text.contains(regia_scene::layers::LEVEL_LAYERS_PATH)
        || text.contains(regia_framework::level_assets::FUNCTIONAL_TEST_PATH)
        || regia_procgen::cook::level_text_may_strip(text)
        || text.contains(regia_asset_pipeline::REVIEW_NOTE_TYPE)
}

/// Writes the level at `source` (either form) to `out` as the package ships it.
fn stage_level(source: &Path, out: &Path, staging: &LevelStaging) -> Result<()> {
    if regia_scene::level_variant::is_level_variant_path(source) {
        return stage_level_variant(source, out, staging);
    }
    let (file, dir) = regia_scene::level_dir::level_paths(source);
    // A path exists in one form only (`ambiguous_level_form`).
    let form = regia_scene::level_dir::level_form(source)
        .with_context(|| format!("staging level {}", source.display()))?;
    // A placed level is found by the id in its sidecar, the only `.meta` a package carries.
    if let Some(meta) = [&dir, &file]
        .map(|path| regia_asset::meta_path_for(path))
        .into_iter()
        .find(|meta| meta.is_file())
    {
        std::fs::copy(&meta, regia_asset::meta_path_for(out))?;
    }
    // A level with layer files (ADR-7792) ships them folded in: the layer files stay out.
    let has_layer_files = !regia_scene::layer_file::list_layer_files(&file)
        .with_context(|| format!("staging level {}", file.display()))?
        .is_empty();
    let scene = if form == regia_scene::level_dir::LevelForm::File && has_layer_files {
        regia_scene::level_dir::read_level_as_stored(&file)
            .with_context(|| format!("folding the layer files of {}", file.display()))?
    } else if form == regia_scene::level_dir::LevelForm::File {
        let text = std::fs::read_to_string(&file)?;
        if !level_needs_staging(&text) {
            std::fs::copy(&file, out)?;
            return Ok(());
        }
        regia_scene::format::SceneFile::from_ron_str(&text)
            .with_context(|| format!("staging level {}", file.display()))?
    } else {
        regia_scene::level_dir::read_level_source(&dir)
            .with_context(|| format!("folding level directory {}", dir.display()))?
    };
    stage_scene(source, &scene, out, staging)
}

/// The cook hook of level variants (ADR-7823): a package has no `.meta` index to find a base by,
/// so a `.levelvariant.ron` ships flattened under its own name (the resolved level as a scene
/// file, which the runtime loader reads as such), staged like any level, with its sidecar.
fn stage_level_variant(source: &Path, out: &Path, staging: &LevelStaging) -> Result<()> {
    let meta = regia_asset::meta_path_for(source);
    if meta.is_file() {
        std::fs::copy(&meta, regia_asset::meta_path_for(out))?;
    }
    let id = regia_framework::level_assets::level_id_of(source).0;
    let find = |base| staging.index.levels.get(&base).cloned();
    let resolved = regia_scene::level_variant::resolve_variant_file(id, source, &find)
        .map_err(|e| anyhow::anyhow!("{}: {e}", e.reason()))
        .with_context(|| format!("resolving level variant {}", source.display()))?;
    stage_scene(source, &resolved.scene, out, staging)
}

/// Writes a level as a package ships it: editor-only layers, functional tests (shipping) and
/// editor-only generators taken out, bound definitions folded in.
fn stage_scene(
    source: &Path,
    scene: &regia_scene::format::SceneFile,
    out: &Path,
    staging: &LevelStaging,
) -> Result<()> {
    // `prepare_level_for_package` asks by id; a bad definition is remembered and stops here.
    let failure = std::cell::RefCell::new(None);
    let defs = |id| match staging.layer_defs(id) {
        Ok(found) => found,
        Err(err) => {
            failure.borrow_mut().get_or_insert(err);
            None
        }
    };
    let packaged =
        regia_framework::level_assets::prepare_level_for_package(scene, &defs, staging.strip_tests);
    if let Some(err) = failure.into_inner() {
        return Err(err.context(format!("staging level {}", source.display())));
    }
    if packaged.tests_stripped > 0 {
        tracing::warn!(
            "{}: {} functional test entities left out of the shipping package",
            source.display(),
            packaged.tests_stripped
        );
    }
    // `editor_only` generators leave the level with their descendants (spec 62 § 7.1, ADR-2421).
    let mut scene = regia_asset_pipeline::strip_review_notes(&packaged.scene);
    let strip = regia_procgen::cook::strip_editor_only(&mut scene);
    if strip.entities > 0 {
        tracing::info!(
            "{}: {} editor-only generators ({} entities) left out of the package",
            source.display(),
            strip.generators.len(),
            strip.entities
        );
    }
    std::fs::write(out, scene.to_ron_string()?)?;
    Ok(())
}

/// Files only functional tests read, left out of shipping packages (spec 41 § 9).
fn is_test_only_asset(name: &str) -> bool {
    [".functest.ron", ".expected.ron", ".session.ron"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// Writes an empty packaged-media index (`regia::PACKAGED_MEDIA_INDEX`) into an Android package
/// that ships no media. A launch prunes the extracted media its index does not list, and takes a
/// package with no index for one built before the index existed, which it walks and never
/// prunes: without the empty index, a version that drops its last clip would leave every
/// earlier clip in the app's storage, still opening by its old path. Desktop packages have no
/// extraction step and get none. Returns how many files it wrote.
///
/// # Errors
/// The index cannot be written.
fn stage_empty_media_index(dest: &Path, platform: &str) -> Result<usize> {
    if platform != "android" {
        return Ok(0);
    }
    std::fs::create_dir_all(dest)?;
    let index = regia::PackagedMediaIndex {
        format_version: 1,
        files: Vec::new(),
    };
    let text = ron::ser::to_string_pretty(&index, regia_core::ron_pretty())?;
    let path = dest.join(regia::PACKAGED_MEDIA_INDEX);
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(1)
}

fn stage_raw_assets(project_dir: &Path, dest: &Path, profile: Profile, platform: &str) -> Result<usize> {
    let assets = project_dir.join("assets");
    if !assets.is_dir() {
        return stage_empty_media_index(dest, platform);
    }
    std::fs::create_dir_all(dest)?;
    let mut count = 0;
    // Buffers and images a `.gltf` references are cooked through it; shipping them raw too would
    // duplicate the mesh data.
    let sources = regia_asset_pipeline::AssetDatabase::discover_sources(&assets);
    let mut sidecars = regia_asset_pipeline::gltf_sidecars(&sources);
    // A project with localization ships normalized bundles (`loc/`, spec 36 § 4) for its shipping
    // locales instead of the authoring `.ftl` files.
    let shipped = stage_loc_bundles(project_dir, dest)?;
    if let Some(shipped) = &shipped {
        count += shipped.len() + 1;
        sidecars.extend(authoring_ftl_files(
            &project_dir.join(super::loc::LOCALIZATION_DIR),
        ));
    }
    // Conversations ship cooked (`<id>.convc`, spec 56 § 4.3) for the shipped locales.
    let cooked = stage_conversations(project_dir, dest, shipped.as_deref())?;
    count += cooked.len();
    sidecars.extend(cooked.into_iter().map(|id| {
        assets
            .join(regia::framework::conversation::asset::CONVERSATIONS_DIR)
            .join(format!("{id}.conv.ron"))
    }));
    count += stage_dialogue_index(project_dir, dest)?;
    let indexed = stage_packaged_media_index(project_dir, dest)?;
    count += indexed;
    if indexed == 0 {
        count += stage_empty_media_index(dest, platform)?;
    }
    let staging = LevelStaging::new(&assets, profile, platform);
    copy_raw_assets_rec(&assets, &assets, dest, &sidecars, &staging, &mut count)?;
    Ok(count)
}

/// Copies the raw files `only` of a named profile's closure into `dest`, shipped the way
/// [`stage_raw_assets`] ships them. A directory-form level is named by its entity files and
/// ships once, as the one `.level.ron` it assembles to.
pub(crate) fn stage_raw_files(
    project_dir: &Path,
    dest: &Path,
    only: &std::collections::BTreeSet<String>,
    profile: Profile,
    platform: &str,
) -> Result<usize> {
    use regia_scene::level_dir::{LEVEL_DIR_SUFFIX, LEVEL_FILE_SUFFIX};
    let assets = project_dir.join("assets");
    let staging = LevelStaging::new(&assets, profile, platform);
    let mut folded = std::collections::BTreeSet::new();
    let mut count = 0;
    for rel in only {
        let level_dir = rel
            .find(&format!("{LEVEL_DIR_SUFFIX}/"))
            .map(|at| &rel[..at + LEVEL_DIR_SUFFIX.len()]);
        let name = rel.rsplit('/').next().unwrap_or(rel);
        if level_dir.is_none() && !staging.ships(name) {
            continue;
        }
        // An ability source never ships raw (ADR-9850 (c)).
        if is_raw_ability_source(Path::new(name)) {
            let message = raw_ability_stripped(rel);
            tracing::warn!(target: "regia_cli", "{message}");
            eprintln!("{message}");
            continue;
        }
        let out = match level_dir {
            Some(dir) if !folded.insert(dir) => continue,
            Some(dir) => dest.join(format!(
                "{}{LEVEL_FILE_SUFFIX}",
                dir.strip_suffix(LEVEL_DIR_SUFFIX).unwrap_or(dir)
            )),
            None => dest.join(rel),
        };
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(dir) = level_dir {
            stage_level(&assets.join(dir), &out, &staging)?;
        } else if name.ends_with(LEVEL_FILE_SUFFIX)
            || name.ends_with(regia_scene::level_variant::LEVEL_VARIANT_SUFFIX)
        {
            stage_level(&assets.join(rel), &out, &staging)?;
        } else {
            let from = assets.join(rel);
            copy_shipped_file(&assets, &from, &out, &staging.platform)
                .with_context(|| format!("copying {}", from.display()))?;
        }
        count += 1;
    }
    // A named profile's package needs the media index as much as the default one does (the
    // Android runtime extracts exactly the listed files). It lists the media this closure staged,
    // stamped from the staged bytes. Like `stage_raw_assets`, an Android closure without media
    // still gets an empty index, so a later version that drops its last clip prunes it (ADR-5922).
    let indexed = write_packaged_media_index(dest, dest)?;
    count += indexed;
    if indexed == 0 {
        count += stage_empty_media_index(dest, platform)?;
    }
    Ok(count)
}

/// Writes the index of the project's dialogue files (`DIALOGUE_INDEX_PATH`): the runtime reads it
/// because an APK cannot be walked. Returns how many files it wrote: none without dialogues.
///
/// # Errors
///
/// A dialogue, voice or cue set does not parse (a package never ships them broken), or the index
/// cannot be written.
fn stage_dialogue_index(project_dir: &Path, dest: &Path) -> Result<usize> {
    use regia::framework::dialogue::{DIALOGUE_INDEX_PATH, DialogueLibrary};
    let assets = project_dir.join("assets");
    let library = DialogueLibrary::load_dir(&assets);
    if !library.errors.is_empty() {
        bail!("dialogue files do not parse:\n{}", library.errors.join("\n"));
    }
    let index = library.index(&assets);
    if index.files.is_empty() {
        return Ok(0);
    }
    let text = ron::ser::to_string_pretty(&index, regia_core::ron_pretty())?;
    let path = dest.join(DIALOGUE_INDEX_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(1)
}

/// Writes the packaged-media index (`regia::PACKAGED_MEDIA_INDEX`) when the project ships raw
/// media files (`.webm`/`.mkv` clips and `.video.ron`/`.videolist.ron` sources, specs/50 § 4):
/// the Android runtime extracts exactly these paths at startup, because an APK assets directory
/// cannot be walked for subdirectories. Each entry carries a (length, FNV-1a) stamp, so a launch
/// skips unchanged clips without reading them. Returns how many files it wrote: none without
/// media.
///
/// # Errors
/// A media file or the index cannot be read or written.
fn stage_packaged_media_index(project_dir: &Path, dest: &Path) -> Result<usize> {
    write_packaged_media_index(&project_dir.join("assets"), dest)
}

/// Writes `dest/`[`regia::PACKAGED_MEDIA_INDEX`] for the media files under `assets` (paths relative
/// to it), as `stage_packaged_media_index` describes. Returns 1, or 0 when there is no media.
///
/// # Errors
/// A media file or the index cannot be read or written.
fn write_packaged_media_index(assets: &Path, dest: &Path) -> Result<usize> {
    let mut files = Vec::new();
    collect_packaged_media(assets, assets, &mut files)?;
    if files.is_empty() {
        return Ok(0);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let index = regia::PackagedMediaIndex {
        format_version: 1,
        files,
    };
    let text = ron::ser::to_string_pretty(&index, regia_core::ron_pretty())?;
    let path = dest.join(regia::PACKAGED_MEDIA_INDEX);
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(1)
}

/// Appends every packaged media file under `dir` (recursively) to `out`, stamped, with paths
/// relative to the assets root. A symlink is left out, as the staging copy leaves it out of the
/// package, and so is a file whose path the runtime would refuse
/// (`regia::is_safe_packaged_media_path`): the index lists only what a launch can extract.
fn collect_packaged_media(root: &Path, dir: &Path, out: &mut Vec<regia::PackagedMediaEntry>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        // `DirEntry::file_type` does not follow symlinks. `copy_raw_assets_rec` does not stage a
        // link (and warns), so the index does not list it, and its target is never read.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_packaged_media(root, &path, out)?;
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !regia::is_packaged_media_name(&name) {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .with_context(|| format!("relativizing {}", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        if !regia::is_safe_packaged_media_path(&relative) {
            tracing::warn!(
                "media file {} is not in {}: a launch extracts only plain relative paths (no `:` or `\\` in a name)",
                path.display(),
                regia::PACKAGED_MEDIA_INDEX
            );
            continue;
        }
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        out.push(regia::PackagedMediaEntry {
            path: relative,
            len: bytes.len() as u64,
            hash: regia::packaged_media_stamp(&bytes),
        });
    }
    Ok(())
}

/// Writes the shipping bundles into `dest/loc/` when the project has `assets/localization/`; the shipped
/// locales, or `None` without localization.
///
/// # Errors
///
/// Localization validation finds errors (a package never ships broken text), or a bundle cannot be written.
fn stage_loc_bundles(project_dir: &Path, dest: &Path) -> Result<Option<Vec<String>>> {
    if !project_dir.join(super::loc::LOCALIZATION_DIR).is_dir() {
        return Ok(None);
    }
    match super::loc_exchange::write_bundles(project_dir, dest, &[], false)? {
        Some((_, shipped)) => Ok(Some(shipped)),
        None => bail!("localization validation failed; run `regia loc validate` for the findings"),
    }
}

/// Cooks every `assets/conversations/<id>.conv.ron` into `dest/conversations/<id>.convc` for the staged
/// locales (the shipped bundle locales, else the source locale) and returns the cooked ids.
///
/// # Errors
///
/// A conversation does not load, a cooked file cannot be written, or a clip is missing in a staged locale
/// while `clip_policy` is `Require` (`conv_missing_clip`).
fn stage_conversations(project_dir: &Path, dest: &Path, shipped: Option<&[String]>) -> Result<Vec<String>> {
    use regia::framework::conversation::{ConversationLibrary, asset::CONVERSATIONS_DIR, cooked};
    let assets = project_dir.join("assets");
    if !assets.join(CONVERSATIONS_DIR).is_dir() {
        return Ok(Vec::new());
    }
    let library = ConversationLibrary::load_assets(&assets).map_err(|e| anyhow::anyhow!("{e}"))?;
    let config = super::loc::read_config(project_dir)?;
    let source = if config.source.is_empty() {
        "en".to_owned()
    } else {
        config.source.clone()
    };
    let locales: Vec<String> = shipped.map_or_else(|| vec![source.clone()], <[String]>::to_vec);
    let loaded: std::collections::BTreeSet<String> = locales.iter().cloned().collect();
    let chain =
        |locale: &str| regia_ui::localization::fallback_chain(locale, &loaded, &config.fallbacks, &source);
    let out_dir = dest.join(CONVERSATIONS_DIR);
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let mut errors = Vec::new();
    let mut ids = Vec::new();
    for (id, asset) in &library.conversations {
        let (cooked, issues) = cooked::cook_conversation(asset, &library, &locales, &chain);
        for issue in issues {
            if issue.error {
                errors.push(format!(
                    "{} {} ({}): {}",
                    issue.code, issue.line, issue.locale, issue.message
                ));
            } else {
                tracing::warn!(
                    "{} {} ({}): {}",
                    issue.code,
                    issue.line,
                    issue.locale,
                    issue.message
                );
            }
        }
        let bytes = cooked.to_bytes().map_err(|e| anyhow::anyhow!("{e}"))?;
        let path = out_dir.join(format!("{id}.{}", cooked::CONVC_EXTENSION));
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
        ids.push(id.clone());
    }
    if !errors.is_empty() {
        bail!("conversation cook failed:\n{}", errors.join("\n"));
    }
    Ok(ids)
}

/// Every `.ftl` file under `dir` (recursively): the authoring files a bundled package leaves out.
fn authoring_ftl_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() && path.extension().is_some_and(|e| e == "ftl") => out.push(path),
                _ => {}
            }
        }
    }
    out
}

fn copy_raw_assets_rec(
    root: &Path,
    dir: &Path,
    dest: &Path,
    sidecars: &std::collections::BTreeSet<PathBuf>,
    staging: &LevelStaging,
    count: &mut usize,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        // `DirEntry::file_type` does not follow symlinks: a link must never pull bytes from
        // outside `assets/` into a shipped package.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            tracing::warn!("skipping symlink in assets: {}", path.display());
            continue;
        }
        if file_type.is_dir() {
            // A level directory (spec 37) is authoring only: it ships as the one `.level.ron` it
            // assembles to, never as entity files (`ship_check` rejects `*.level.d`).
            if let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".level.d")) {
                let out = dest.join(format!("{stem}.level.ron"));
                std::fs::create_dir_all(dest)?;
                stage_level(&path, &out, staging)?;
                *count += 1;
                continue;
            }
            copy_raw_assets_rec(root, &path, &dest.join(&name), sidecars, staging, count)?;
            continue;
        }
        // Relative to the assets root, for cookable/sidecar detection.
        let rel = path
            .strip_prefix(root)
            .context("asset path escaped the assets root")?;
        if rel.extension().is_some_and(|e| e == "meta") {
            continue;
        }
        // Before the shipping rules: an ability source never ships raw (ADR-9850 (c)).
        if is_raw_ability_source(rel) {
            let message = raw_ability_stripped(&rel.to_string_lossy().replace('\\', "/"));
            tracing::warn!(target: "regia_cli", "{message}");
            eprintln!("{message}");
            continue;
        }
        if (is_cookable_source(rel) && !ships_raw_too(rel)) || sidecars.contains(&path) {
            continue;
        }
        let file_name = name.to_string_lossy();
        if !staging.ships(&file_name) {
            continue;
        }
        // Layer files are folded into their level by `stage_level` (ADR-7792).
        if file_name.ends_with(regia_scene::layer_file::LAYER_FILE_SUFFIX) {
            continue;
        }
        let out = dest.join(&name);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if file_name.ends_with(regia_scene::level_dir::LEVEL_FILE_SUFFIX)
            || file_name.ends_with(regia_scene::level_variant::LEVEL_VARIANT_SUFFIX)
        {
            stage_level(&path, &out, staging)?;
        } else {
            copy_shipped_file(root, &path, &out, &staging.platform)?;
        }
        *count += 1;
    }
    Ok(())
}

/// Copies one raw asset into a package. A sound the project's audio cook policy transcodes
/// (`audio_cook.toml`, ADR-3230) is written as Ogg Vorbis under its own name; everything else is
/// a plain copy.
fn copy_shipped_file(assets_root: &Path, from: &Path, out: &Path, platform: &str) -> Result<()> {
    // A prefab saved from the isolated editor scene can carry review markers too.
    if from
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(".prefab.ron"))
    {
        let text = std::fs::read_to_string(from)?;
        if text.contains(regia_asset_pipeline::REVIEW_NOTE_TYPE) {
            let scene = regia_scene::format::SceneFile::from_ron_str(&text)
                .with_context(|| format!("staging prefab {}", from.display()))?;
            let scene = regia_asset_pipeline::strip_review_notes(&scene);
            std::fs::write(out, scene.to_ron_string()?)?;
            return Ok(());
        }
    }
    if let Some(project_dir) = assets_root.parent() {
        let rel = from.strip_prefix(assets_root).unwrap_or(from);
        let rel = format!("assets/{}", rel.to_string_lossy().replace('\\', "/"));
        match regia_asset_pipeline::audio_transcode::shipped_audio_bytes(project_dir, platform, from, &rel) {
            Ok(Some(bytes)) => {
                std::fs::write(out, bytes)?;
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => anyhow::bail!("audio transcode: {e}"),
        }
    }
    std::fs::copy(from, out)?;
    Ok(())
}

/// The engine's `THIRD_PARTY_NOTICES.md`, compiled in: a game in its own workspace (every
/// `regia new` project) has no copy at its root, and its package still ships the notices of the
/// crates the engine links (ADR-1379).
const ENGINE_NOTICES: &str = include_str!("../../assets/THIRD_PARTY_NOTICES.md");

/// Copy the third-party notices into `dest` (`docs/07-licensing.md` § 6: notices ship with every
/// build), looked up at the workspace root, and write the engine's MPL notice, licence text and the
/// credits file ([`package_legal::write`], ADR-10225).
///
/// `sbom_targets` are the target triples the package's binaries are built for ([`sbom_triples`]); the
/// SBOM lists the packages they resolve. `no_engine_credit` is `--no-engine-credit`.
pub(crate) fn copy_notices(
    dest: &Path,
    project_dir: &Path,
    sbom_targets: &[String],
    no_engine_credit: bool,
) -> Result<()> {
    let legal = package_legal::Legal::resolve(project_dir, no_engine_credit)?;
    package_legal::write(dest, project_dir, &legal)?;
    let workspace = cargo_workspace_root(project_dir)?;
    if copy_notices_from(&workspace, dest)? {
        tracing::warn!(
            "{} has no THIRD_PARTY_NOTICES.md: the package ships the engine's notices from this regia \
             build; add the notices of your own dependencies to it before you ship",
            workspace.display()
        );
    }
    crate::commands::package_sbom::write_sbom(dest, &workspace, sbom_targets)?;
    Ok(())
}

/// [`copy_notices`] for a known workspace root. A game's own `LICENSE.md` is copied when its workspace
/// has one (a game's own licence is its business; the engine's is written by [`package_legal`]). `THIRD_PARTY_NOTICES.md` is copied, or, when the
/// workspace has none, written from the compiled-in engine copy: returns `true` then.
fn copy_notices_from(workspace: &Path, dest: &Path) -> Result<bool> {
    let license = workspace.join("LICENSE.md");
    if license.is_file() {
        std::fs::copy(&license, dest.join("LICENSE.md"))
            .with_context(|| format!("copying {}", license.display()))?;
    }
    let notices = workspace.join("THIRD_PARTY_NOTICES.md");
    if notices.is_file() {
        std::fs::copy(&notices, dest.join("THIRD_PARTY_NOTICES.md"))
            .with_context(|| format!("copying {}", notices.display()))?;
        return Ok(false);
    }
    let out = dest.join("THIRD_PARTY_NOTICES.md");
    std::fs::write(&out, ENGINE_NOTICES).with_context(|| format!("writing {}", out.display()))?;
    Ok(true)
}

/// The icon files to try, in order: the `[platforms.android]` key when it is set (a missing file
/// is reported and skipped), then the conventional locations (specs/21 § 2, ADR-5627).
fn android_icon_candidates(project_dir: &Path, configured: Option<&str>, defaults: &[&str]) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = configured.map(str::trim).filter(|path| !path.is_empty()) {
        let path = project_dir.join(path);
        if !path.is_file() {
            tracing::warn!(
                icon = %path.display(),
                "the configured Android icon is missing; trying the default locations"
            );
        }
        candidates.push(path);
    }
    candidates.extend(defaults.iter().map(|path| project_dir.join(path)));
    candidates
}

/// Android density buckets with the launcher icon size (48 dp) and the adaptive-icon foreground
/// size (108 dp) in pixels: mdpi is 1x, hdpi 1.5x, xhdpi 2x, xxhdpi 3x, xxxhdpi 4x.
const MIPMAP_DENSITIES: [(&str, u32, u32); 5] = [
    ("mipmap-mdpi", 48, 108),
    ("mipmap-hdpi", 72, 162),
    ("mipmap-xhdpi", 96, 216),
    ("mipmap-xxhdpi", 144, 324),
    ("mipmap-xxxhdpi", 192, 432),
];

/// Background of the generated placeholder icon and of the adaptive icon (`ic_launcher_background`).
const ICON_BACKGROUND: [u8; 4] = [0x1A, 0x1A, 0x2E, 0xFF];

/// The placeholder launcher icon when a project has none: the background colour with a lighter
/// centred square, so the APK always carries `@mipmap/ic_launcher`.
fn placeholder_icon(size: u32) -> image::DynamicImage {
    let inset = size / 4;
    let buffer = image::RgbaImage::from_fn(size, size, |x, y| {
        let inside = (inset..size - inset).contains(&x) && (inset..size - inset).contains(&y);
        image::Rgba(if inside {
            [0xE9, 0x45, 0x60, 0xFF]
        } else {
            ICON_BACKGROUND
        })
    });
    image::DynamicImage::ImageRgba8(buffer)
}

/// Writes `res/<bucket>/<name>.png` for every density, resampled from `source`.
fn write_mipmaps(
    res: &Path,
    name: &str,
    source: &image::DynamicImage,
    size_of: fn(&(&str, u32, u32)) -> u32,
) -> Result<()> {
    for density in &MIPMAP_DENSITIES {
        let size = size_of(density);
        let dir = res.join(density.0);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{name}.png"));
        source
            .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
            .save_with_format(&path, image::ImageFormat::Png)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Decodes the first existing candidate, or `None` when there is none. A file that is not a
/// readable image fails the package (AAPT2 would refuse it later with a less useful message).
fn load_android_icon(candidates: &[PathBuf], what: &str) -> Result<Option<image::DynamicImage>> {
    let Some(path) = candidates.iter().find(|path| path.is_file()) else {
        return Ok(None);
    };
    let icon = image::open(path)
        .with_context(|| format!("the Android {what} {} is not a readable PNG", path.display()))?;
    if icon.width() != icon.height() {
        tracing::warn!(
            icon = %path.display(),
            width = icon.width(),
            height = icon.height(),
            "the Android {what} is not square; it is stretched to each density's square"
        );
    }
    tracing::info!(icon = %path.display(), "using the Android {what}");
    Ok(Some(icon))
}

/// Writes the launcher icon into every Gradle mipmap density (specs/21 § 4 Android step 2,
/// ADR-5627): `ic_launcher.png` from `[platforms.android] icon` (else `assets/branding/icon.png`,
/// `assets/icon.png`, else a generated placeholder, so the manifest's `@mipmap/ic_launcher` always
/// resolves), and with `adaptive_icon_foreground` (else `assets/branding/icon_fg.png`,
/// `assets/branding/icon_foreground.png`) an adaptive icon: `ic_launcher_foreground.png` per density
/// and `mipmap-anydpi-v26/ic_launcher.xml` over the `ic_launcher_background` colour.
fn copy_android_icons(project_dir: &Path, gradle_dir: &Path, android: &AndroidPlatform) -> Result<()> {
    let res = gradle_dir.join("app/src/main/res");
    let launcher = android_icon_candidates(
        project_dir,
        android.icon.as_deref(),
        &["assets/branding/icon.png", "assets/icon.png"],
    );
    let launcher = match load_android_icon(&launcher, "icon")? {
        Some(icon) => icon,
        None => {
            tracing::warn!("no Android icon found; packaging a placeholder (set [platforms.android] icon)");
            placeholder_icon(192)
        }
    };
    write_mipmaps(&res, "ic_launcher", &launcher, |density| density.1)?;

    let foreground = android_icon_candidates(
        project_dir,
        android.adaptive_icon_foreground.as_deref(),
        &[
            "assets/branding/icon_fg.png",
            "assets/branding/icon_foreground.png",
        ],
    );
    let adaptive = res.join("mipmap-anydpi-v26");
    let Some(foreground) = load_android_icon(&foreground, "adaptive icon foreground")? else {
        // A stale adaptive icon from an earlier package would hide the launcher icon.
        let _ = std::fs::remove_file(adaptive.join("ic_launcher.xml"));
        return Ok(());
    };
    write_mipmaps(&res, "ic_launcher_foreground", &foreground, |density| density.2)?;
    std::fs::create_dir_all(&adaptive)?;
    std::fs::write(
        adaptive.join("ic_launcher.xml"),
        r#"<?xml version="1.0" encoding="utf-8"?>
<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">
    <background android:drawable="@color/ic_launcher_background"/>
    <foreground android:drawable="@mipmap/ic_launcher_foreground"/>
</adaptive-icon>
"#,
    )?;
    let values = res.join("values");
    std::fs::create_dir_all(&values)?;
    let colors_xml = values.join("colors.xml");
    if !colors_xml.is_file() {
        std::fs::write(
            &colors_xml,
            r#"<?xml version="1.0" encoding="utf-8"?>
<resources>
    <color name="ic_launcher_background">#1A1A2E</color>
</resources>
"#,
        )?;
    }
    Ok(())
}

/// Icon path from the project manifest.
fn icon_for_project(_project_dir: &Path, _manifest: &Manifest) -> Option<PathBuf> {
    // Icons are in the full spec manifest fields that we parse as optional/unknown.
    // For now, check the common location.
    let path = _project_dir.join("assets/branding/icon.png");
    if path.is_file() {
        return Some(path);
    }
    let path = _project_dir.join("assets/icon.png");
    if path.is_file() {
        return Some(path);
    }
    None
}

/// How shipping debug symbols are produced for a platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolFormat {
    /// ELF DWARF (Linux): extract with `objcopy --only-keep-debug`.
    Dwarf,
    /// Windows PDB produced by the MSVC linker.
    Pdb,
}

/// Keeps a symbolication artifact for a shipping build under `dist/<p>/symbols/` and strips the
/// staged binary where applicable (`docs/specs/21-tooling-and-packaging.md` § 4.4).
///
/// `[profile.shipping]` builds with `debug = "line-tables-only"` and `strip = "none"`, so the built
/// binary still carries the symbol table and line tables. For Linux the package extracts them with
/// `objcopy --only-keep-debug <source> <symbols>/<name>.debug` (keeping the DWARF's address
/// relocations, which a `split-debuginfo = "packed"` `.dwp` loses once the binary is stripped) and
/// then strips the staged copy with `objcopy --strip-all`. Symbolicate a shipped backtrace with
/// `addr2line -C -e symbols/<name>.debug -f <address>`.
///
/// For Windows the MSVC linker's `.pdb` is copied to `symbols/<name>.pdb` and resolves addresses in
/// WinDbg/Visual Studio. The Windows exe is **not** stripped: MSVC writes all debug info (CodeView)
/// into the separate `.pdb`, so there is nothing embedded in the exe to strip — `strip = "none"` is
/// correct there and keeps the exe's reference to its `.pdb` intact. (Android shipping builds do not
/// come through here: `android::symbols::collect_symbols` copies each ABI's unstripped
/// `lib<name>.so` out of the cargo-ndk output into `dist/android/symbols/<abi>/` with `symbols.json`
/// before Gradle strips the APK's copy — specs/21 § 4, ADR-2631.)
///
/// Dev builds keep debug info in the binary and produce no `symbols/` directory.
///
/// A missing artifact fails the package (it is not a warning): a shipping build must ship its
/// symbols, or crash reports cannot be symbolicated.
pub(crate) fn keep_shipping_symbols(
    source: &Path,
    staged: &Path,
    out_dir: &Path,
    profile: Profile,
    format: SymbolFormat,
) -> Result<()> {
    if profile != Profile::Shipping {
        return Ok(());
    }
    let symbols_dir = out_dir.join("symbols");
    std::fs::create_dir_all(&symbols_dir)?;
    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("symbol source has no file name"))?;
    match format {
        SymbolFormat::Dwarf => {
            let debug_file = symbols_dir.join(format!("{}.debug", file_name.to_string_lossy()));
            process::run(
                Command::new("objcopy")
                    .arg("--only-keep-debug")
                    .arg(source)
                    .arg(&debug_file),
                "Install binutils (`objcopy`) to extract the shipping debug symbols",
            )
            .with_context(|| format!("extracting debug symbols from {}", source.display()))?;
            process::run(
                Command::new("objcopy").arg("--strip-all").arg(staged),
                "Install binutils (`objcopy`) to strip the shipped binary",
            )
            .with_context(|| format!("stripping {}", staged.display()))?;
            if !debug_file.is_file() {
                bail!("objcopy reported success but {} is missing", debug_file.display());
            }
            tracing::info!("symbols: {}", debug_file.display());
        }
        SymbolFormat::Pdb => {
            let candidates = pdb_candidates(source);
            let Some(symbol_file) = candidates.iter().find(|p| p.is_file()).cloned() else {
                bail!(
                    "shipping symbols not found at {} (build on Windows/MSVC to produce the .pdb)",
                    candidates
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(" or ")
                );
            };
            // Store the PDB under its own file name (`symbols/<name>.pdb`), not the exe name:
            // specs/21 § 4.4 and the packaging guide both promise `symbols/<name>.pdb`.
            let pdb_name = symbol_file
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("symbol file has no file name"))?;
            let symbols_path = symbols_dir.join(pdb_name);
            std::fs::copy(&symbol_file, &symbols_path)?;
            tracing::info!("symbols: {}", symbols_path.display());
        }
    }
    Ok(())
}

/// Where the MSVC linker's PDB for `exe` may be: cargo names it after the crate, so `my-game.exe`
/// has `my_game.pdb` (hyphens become underscores), then the exe's own stem (ADR-868).
fn pdb_candidates(exe: &Path) -> Vec<PathBuf> {
    let stem = exe
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut candidates = vec![exe.with_file_name(format!("{}.pdb", stem.replace('-', "_")))];
    let own = exe.with_extension("pdb");
    if !candidates.contains(&own) {
        candidates.push(own);
    }
    candidates
}

/// Write `build_info.json`. `git` is the revision of the project's source tree (the staging
/// directory is outside it); `extra` adds platform fields (Linux: `glibc_floor`, `glibc_max`).
/// Copies the shipped pipeline caches (`--pipeline-caches`, default `<project>/pipeline-caches`)
/// into `<content>/pipelines/`, each under its key's shipped name, and returns one JSON entry per
/// file for `build_info.json`. Every file must be a valid, intact cache (header, key, checksum):
/// a broken one fails the package rather than ship. Files with the same key keep the larger.
fn stage_pipeline_caches(
    project_dir: &Path,
    args: &PackageArgs,
    content: &Path,
) -> Result<Vec<serde_json::Value>> {
    let explicit = args.pipeline_caches.is_some();
    let dir = args
        .pipeline_caches
        .clone()
        .unwrap_or_else(|| project_dir.join("pipeline-caches"));
    if !dir.exists() {
        if explicit {
            bail!("--pipeline-caches {} does not exist", dir.display());
        }
        return Ok(Vec::new());
    }
    let mut staged = collect_pipeline_caches(&dir)?;
    let mut references = std::collections::BTreeSet::new();
    for path in &args.pipeline_cache_references {
        let caches = collect_pipeline_caches(path)?;
        if caches.is_empty() {
            bail!("pipeline cache reference {} contains no cache", path.display());
        }
        references.extend(
            caches
                .into_values()
                .map(|(_, key, _)| (key.shader_hash, key.wgpu_version)),
        );
    }
    staged.retain(|name, (source, key, _)| {
        let matches = if references.is_empty() {
            source.entry.is_none() || cache_matches_engine(key)
        } else {
            references.contains(&(key.shader_hash.clone(), key.wgpu_version.clone()))
        };
        if !matches {
            eprintln!("pipeline cache skipped: {name} (shader/wgpu does not match this build's references)");
        }
        matches
    });
    if staged.is_empty() {
        return Ok(Vec::new());
    }
    let out = content.join("pipelines");
    std::fs::create_dir_all(&out)?;
    let mut entries = Vec::new();
    let mut stale = 0usize;
    for (name, (source, key, bytes)) in &staged {
        let data = source.read()?;
        package_pipeline_caches::write_atomic(&out.join(name), &data)
            .with_context(|| format!("staging {}", source.path.display()))?;
        let matches = cache_matches_engine(key);
        if !matches && references.is_empty() {
            stale += 1;
            eprintln!(
                "WARNING: pipeline cache {} was built for another engine (shaders {}, wgpu {}; this \
                 engine: shaders {}, wgpu {}): it ships but never seeds a player's cache unless the \
                 game sets a content salt that explains the difference. Rebuild it from this build.",
                source.path.display(),
                key.shader_hash,
                key.wgpu_version,
                regia_render::pipeline_cache::engine_shader_hash(),
                regia_render::pipeline_cache::WGPU_VERSION,
            );
        }
        outln!(
            "pipeline cache: {name} ({} {:#06x}:{:#06x}, driver {}, {} KiB)",
            key.driver,
            key.vendor,
            key.device,
            key.driver_info,
            bytes / 1024
        );
        entries.push(serde_json::json!({
            "file": format!("content/pipelines/{name}"),
            "vendor": format!("{:#06x}", key.vendor),
            "device": format!("{:#06x}", key.device),
            "driver": key.driver,
            "driver_info": key.driver_info,
            "wgpu": key.wgpu_version,
            "shaders": key.shader_hash,
            "matches_engine": matches,
            "bytes": bytes,
        }));
    }
    if stale > 0 {
        eprintln!(
            "WARNING: {stale} of {} shipped pipeline caches do not match this engine's shaders or wgpu \
             version (build_info.json: pipeline_caches[].matches_engine = false)",
            staged.len()
        );
    }
    Ok(entries)
}

/// Whether a shipped cache was produced by this engine: the same wgpu version and the unsalted
/// shader hash ([`regia_render::pipeline_cache::engine_shader_hash`]). A salted cache (a game
/// that calls `set_content_salt`) does not match here and is reported, not refused.
fn cache_matches_engine(key: &regia_render::pipeline_cache::PipelineCacheKey) -> bool {
    key.wgpu_version == regia_render::pipeline_cache::WGPU_VERSION
        && key.shader_hash == regia_render::pipeline_cache::engine_shader_hash()
}

/// Validates loose cache files and stored feedback ZIPs, including nested directories.
/// Maps shipped filename to source, exact key and encoded size. Largest valid blob wins;
/// equal sizes use the lowest content hash. Sources are rechecked when staged.
pub fn collect_pipeline_caches(path: &Path) -> Result<package_pipeline_caches::CollectedCaches> {
    package_pipeline_caches::collect(path)
}

/// The `prepare_maps` field of `build_info.json` (ADR-8453): one `{ name, path }` per
/// `--prepare-scene`, in order; `path` is project-relative with `/` separators, `name` is what
/// `regia prepare --scene` calls the map. Every level must exist under the project.
pub(crate) fn prepare_maps(project_dir: &Path, scenes: &[PathBuf]) -> Result<serde_json::Value> {
    let mut maps = Vec::with_capacity(scenes.len());
    for scene in scenes {
        let path = if scene.is_absolute() {
            scene.clone()
        } else {
            project_dir.join(scene)
        };
        if !path.exists() {
            bail!("--prepare-scene {}: no level source there", scene.display());
        }
        let relative = path.strip_prefix(project_dir).map_err(|_| {
            anyhow::anyhow!(
                "--prepare-scene {}: the level must be inside the project",
                scene.display()
            )
        })?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        let name = [".level.ron", ".level.d"]
            .iter()
            .find_map(|suffix| relative.strip_suffix(suffix))
            .unwrap_or(&relative)
            .to_string();
        maps.push(serde_json::json!({ "name": name, "path": relative }));
    }
    Ok(serde_json::Value::Array(maps))
}

/// Adds `"render_profile"` to the `build_info.json` fields when `[platforms.<platform>]` sets one
/// (`MobileHigh`, `DesktopHigh`, ...; canonical spelling, validated when the manifest loads).
pub(crate) fn insert_render_profile(
    extra: &mut serde_json::Map<String, serde_json::Value>,
    manifest: &Manifest,
    platform: &str,
) {
    if let Some(profile) = manifest.platforms.render_profile(platform) {
        extra.insert("render_profile".into(), profile.into());
    }
}

pub(crate) fn write_build_info(
    staging: &Path,
    project_dir: &Path,
    manifest: &Manifest,
    extra: serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let mut info = serde_json::json!({
        "version": manifest.project.version,
        "build_number": manifest.project.build_number,
        "engine": env!("CARGO_PKG_VERSION"),
        "git": git_revision(project_dir),
        "date": chrono_like_date(),
    });
    if let Some(object) = info.as_object_mut() {
        object.extend(extra);
        if let Some(boot) = &manifest.boot {
            object.insert("boot".to_string(), serde_json::to_value(boot)?);
        }
        // The pause key and its screen (ADR-10104): read by `GamePausePlugin` without a manifest.
        if let Some(game) = &manifest.game {
            object.insert("game".to_string(), serde_json::to_value(game)?);
        }
    }
    std::fs::write(
        staging.join("build_info.json"),
        serde_json::to_string_pretty(&info)?,
    )?;
    Ok(())
}

fn chrono_like_date() -> String {
    // Simple date without adding chrono dependency.
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Approximate: not perfect but works for dates after 1970.
    let days = secs / 86400;
    // Algorithm from Howard Hinnant (already used in regia_core).
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Where `<project>/assets/**` lands inside the Gradle project: the APK's `assets/` folder
/// (relative to the Gradle root, `/`-separated).
pub const APK_ASSETS_DIR: &str = "app/src/main/assets";

/// Recursively copies `from` into `to` (creating it), returning the number of files copied.
#[allow(dead_code)]
pub fn copy_dir(from: &Path, to: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(to)?;
    let mut count = 0;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let dest = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            tracing::warn!("skipping symlink in assets: {}", source.display());
        } else if file_type.is_dir() {
            count += copy_dir(&source, &dest)?;
        } else {
            std::fs::copy(&source, &dest)?;
            count += 1;
        }
    }
    Ok(count)
}

/// `<project>/build/android`: the generated Gradle project.
pub fn gradle_dir(project_dir: &Path) -> PathBuf {
    project_dir.join("build").join("android")
}

/// `<project>/dist/<platform>`: default output directory.
pub fn default_out_dir(project_dir: &Path, platform: &str) -> PathBuf {
    project_dir.join("dist").join(platform)
}

/// File name of the debug APK for an application id.
pub fn debug_apk_name(app_id: &str) -> String {
    let slug = slugify(app_id).unwrap_or_else(|| "app".to_string());
    format!("{slug}-debug.apk")
}

/// `manifest` with `[platforms.android] abis` narrowed to `requested` (kept in manifest order).
/// An empty `requested` keeps every ABI; an ABI the manifest does not list is an error.
pub fn with_android_abis(manifest: &Manifest, requested: &[String]) -> Result<Manifest> {
    let mut manifest = manifest.clone();
    if requested.is_empty() {
        return Ok(manifest);
    }
    let listed = &manifest.platforms.android.abis;
    if let Some(unknown) = requested.iter().find(|abi| !listed.contains(abi)) {
        bail!(
            "--abi {unknown} is not in [platforms.android] abis ({}) of Regia.toml",
            listed.join(", ")
        );
    }
    manifest
        .platforms
        .android
        .abis
        .retain(|abi| requested.contains(abi));
    Ok(manifest)
}

/// Arguments for `cargo` that build the Android cdylib for every ABI into `jni_libs_dir`.
pub fn cargo_ndk_args(android: &AndroidPlatform, jni_libs_dir: &Path, profile: Profile) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["ndk".into()];
    for abi in &android.abis {
        args.push("-t".into());
        args.push(abi.into());
    }
    args.push("-P".into());
    args.push(android.min_sdk.to_string().into());
    args.push("-o".into());
    args.push(jni_libs_dir.into());
    args.push("build".into());
    if profile == Profile::Shipping {
        args.push("--profile".into());
        args.push("shipping".into());
    }
    args.push("-p".into());
    args.push(android.cargo_package.clone().into());
    args
}

/// The shipping profile of specs/21 § 4 (common step 4), as `profile.shipping.<key>` TOML values.
/// The templates' `Cargo.toml` and the engine workspace define the same table.
const SHIPPING_PROFILE: [(&str, &str); 6] = [
    ("inherits", "\"release\""),
    ("lto", "\"fat\""),
    ("codegen-units", "1"),
    ("panic", "\"abort\""),
    ("debug", "\"line-tables-only\""),
    ("strip", "\"none\""),
];

/// `--config` values that define the shipping profile when the workspace manifest has no
/// `[profile.shipping]` (ADR-5627). Projects generated before the templates carried the table
/// lack it, and cargo refuses `--profile shipping` with "profile `shipping` is not defined". A
/// manifest that defines the profile is left alone, so a project's own settings win.
pub(crate) fn shipping_profile_config(workspace: &Path) -> Vec<String> {
    let manifest = workspace.join("Cargo.toml");
    if manifest_defines_shipping_profile(&manifest) {
        return Vec::new();
    }
    tracing::warn!(
        "{} has no [profile.shipping]; building with the engine's shipping settings \
         (add the table to keep your own, see specs/21 § 4)",
        manifest.display()
    );
    SHIPPING_PROFILE
        .iter()
        .map(|(key, value)| format!("profile.shipping.{key}={value}"))
        .collect()
}

/// True when the manifest at `path` has a `[profile.shipping]` table. An unreadable or invalid
/// manifest counts as no table (cargo reports the manifest error itself).
fn manifest_defines_shipping_profile(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(table) = toml::from_str::<toml::Table>(&text) else {
        return false;
    };
    table
        .get("profile")
        .and_then(|profiles| profiles.get("shipping"))
        .is_some_and(toml::Value::is_table)
}

/// True when the project's Regia.toml names `[project] version`. `Manifest::project` fills the
/// serde default when the key is absent, and the Windows templates' `build.rs` then embeds
/// `CARGO_PKG_VERSION` — a version check would compare against a number nobody set.
fn project_sets_version(project_dir: &Path) -> bool {
    std::fs::read_to_string(project_dir.join("Regia.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .is_some_and(|table| table.get("project").is_some_and(|p| p.get("version").is_some()))
}

/// Root of the cargo workspace containing `project_dir`.
pub fn cargo_workspace_root(project_dir: &Path) -> Result<PathBuf> {
    let (stdout, _) = process::output(
        Command::new("cargo")
            .args(["locate-project", "--workspace", "--message-format", "plain"])
            .current_dir(project_dir),
        "Install Rust with rustup (https://rustup.rs)",
    )
    .with_context(|| format!("{} is not inside a cargo workspace", project_dir.display()))?;
    let manifest = PathBuf::from(stdout.trim());
    manifest
        .parent()
        .map(Path::to_path_buf)
        .with_context(|| format!("unexpected `cargo locate-project` output: {stdout}"))
}

/// Builds a debug APK for the project in `project_dir` (used by `install-android`), for `abis`
/// (empty: every ABI in the manifest).
pub fn package_android_debug_apk(
    project_dir: &Path,
    out_dir: Option<&Path>,
    abis: &[String],
) -> Result<PathBuf> {
    let project_dir = project_dir
        .canonicalize()
        .with_context(|| format!("project directory {} not found", project_dir.display()))?;
    let _manifest = Manifest::load(&project_dir)?;
    let args = PackageArgs {
        platform: Platform::Android,
        format: Some(Format::Apk),
        profile_name: "dev".to_string(),
        profile: Profile::Dev,
        dry_run: false,
        project: project_dir.clone(),
        out: out_dir.map(Path::to_path_buf),
        steam: false,
        abis: abis.to_vec(),
        glibc: None,
        pipeline_caches: None,
        pipeline_cache_references: Vec::new(),
        prepare_scenes: Vec::new(),
        server: false,
        json: false,
        audit_dir: None,
        audit_artifact: None,
        verify_receipt: None,
        container: None,
        container_image: None,
        container_mounts: Vec::new(),
        no_engine_credit: false,
    };
    run(&args)
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: PackageArgs,
    }

    fn parse(argv: &[&str]) -> Result<PackageArgs> {
        let full = std::iter::once("regia-package").chain(argv.iter().copied());
        let mut args = Wrapper::try_parse_from(full)?.args;
        // `run` resolves the name against the manifest; the built-in names need no manifest.
        if args.profile_name == "shipping" {
            args.profile = Profile::Shipping;
        }
        Ok(args)
    }

    #[test]
    fn powershell_paths_with_quotes_stay_one_literal() {
        assert_eq!(powershell_quote("C:\\out\\It's"), "'C:\\out\\It''s'");
    }

    #[test]
    fn cargo_target_uses_bin_name_and_skips_virtual_workspaces() {
        assert_eq!(parse_cargo_target("[workspace]\nmembers = [\"a\"]\n"), None);
        let plain = parse_cargo_target("[package]\nname = \"g\"\n").expect("test setup");
        assert_eq!((plain.package.as_str(), plain.bin.as_str()), ("g", "g"));
        let bin = parse_cargo_target(
            "[package]\nname = \"g_launcher\"\n[[bin]]\nname = \"g\"\npath = \"src/main.rs\"\n",
        )
        .expect("test setup");
        assert_eq!((bin.package.as_str(), bin.bin.as_str()), ("g_launcher", "g"));
    }

    #[test]
    fn generated_project_resolves_to_its_launcher() {
        let dir = regia_test_utils::unique_temp_dir("pkg_launcher");
        std::fs::create_dir_all(dir.join("launcher")).expect("test setup");
        std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = [\"launcher\"]\n")
            .expect("test setup");
        std::fs::write(
            dir.join("launcher").join("Cargo.toml"),
            "[package]\nname = \"g_launcher\"\n[[bin]]\nname = \"g\"\n",
        )
        .expect("test setup");
        let t = cargo_package_name(&dir).expect("test setup");
        assert_eq!((t.package.as_str(), t.bin.as_str()), ("g_launcher", "g"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_staging_dir_is_cleared_before_the_new_package() {
        let dir = regia_test_utils::unique_temp_dir("pkg_stale");
        let staging = dir.join("Game");
        std::fs::create_dir_all(staging.join("content")).expect("test setup");
        // Files a previous package left that stage_content would not clear: an old binary, a
        // Steam marker, a removed project icon.
        std::fs::write(staging.join("game"), b"stale binary").expect("test setup");
        std::fs::write(staging.join("steam_appid.txt"), b"123").expect("test setup");
        std::fs::write(staging.join("icon.png"), b"stale icon").expect("test setup");
        fresh_staging(&staging).expect("fresh staging");
        assert!(
            !staging.join("game").exists(),
            "the stale binary must not survive into the new package"
        );
        assert!(
            !staging.join("steam_appid.txt").exists(),
            "the stale steam marker must not survive"
        );
        assert!(
            !staging.join("icon.png").exists(),
            "the removed icon must not survive"
        );
        assert!(staging.join("content").is_dir(), "the content dir is recreated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_tarball_write_preserves_the_previous_archive() {
        let dir = regia_test_utils::unique_temp_dir("tarball_atomic");
        std::fs::create_dir_all(dir.join("stage")).expect("test setup");
        std::fs::write(dir.join("stage").join("asset.bin"), b"payload").expect("test setup");
        // Staging carries the SBOM sidecar, as `copy_notices` writes it, so the seal check passes.
        std::fs::write(dir.join("stage").join("sbom.json"), b"{}").expect("test setup");
        let tarball = dir.join("game-linux.tar.gz");

        // A first successful write produces the good archive and leaves no temp file behind.
        write_tarball(&tarball, &dir, "stage").expect("first tarball");
        let good = std::fs::read(&tarball).expect("read good archive");
        assert!(!good.is_empty());
        let part = PathBuf::from(format!("{}.part", tarball.display()));
        assert!(!part.exists(), "no `.part` left after a successful write");

        // A failing write (tar cannot find the input directory) must not truncate or replace the
        // good archive: `tar -czf` opens its output first, so before the fix this truncated the
        // archive in place. The previous good archive must be preserved.
        let err = write_tarball(&tarball, &dir, "no-such-dir").expect_err("tar fails");
        assert!(err.to_string().contains("creating the tarball"), "{err}");
        assert_eq!(
            std::fs::read(&tarball).expect("read again"),
            good,
            "the previous good archive is preserved"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn binary_lookup_follows_cargo_target_dir_first() {
        let ws = Path::new("/ws");
        let project = Path::new("/ws/samples/game");
        let found = binary_candidates(
            ws,
            project,
            Some(Path::new("/farm/target")),
            None,
            "debug",
            "game",
        );
        assert_eq!(
            found,
            [
                PathBuf::from("/farm/target/debug/game"),
                PathBuf::from("/ws/target/debug/game"),
                PathBuf::from("/ws/samples/game/target/debug/game"),
            ]
        );
        let relative = binary_candidates(ws, project, Some(Path::new("out")), None, "shipping", "game");
        assert_eq!(relative[0], PathBuf::from("/ws/out/shipping/game"));
        assert_eq!(
            binary_candidates(ws, project, None, None, "debug", "game").len(),
            2
        );
        assert_eq!(
            binary_candidates(ws, project, Some(Path::new("")), None, "debug", "game").len(),
            2
        );
    }

    #[test]
    fn binary_lookup_prefers_the_cross_target_directory() {
        let ws = Path::new("/ws");
        let project = Path::new("/ws/samples/game");
        let found = binary_candidates(
            ws,
            project,
            None,
            Some(WINDOWS_MSVC_TARGET),
            "shipping",
            "game.exe",
        );
        assert_eq!(
            found[0],
            PathBuf::from(format!("/ws/target/{WINDOWS_MSVC_TARGET}/shipping/game.exe"))
        );
        assert_eq!(found[1], PathBuf::from("/ws/target/shipping/game.exe"));
        assert_eq!(found.len(), 4);
    }

    #[test]
    fn windows_cross_build_uses_cargo_xwin() {
        if cfg!(windows) {
            assert!(cross_target(Platform::Windows).is_none());
        } else {
            assert_eq!(cross_target(Platform::Windows), Some(WINDOWS_MSVC_TARGET));
            assert!(use_cargo_xwin(Some(WINDOWS_MSVC_TARGET)));
        }
        assert!(cross_target(Platform::Linux).is_none());
        assert!(!use_cargo_xwin(None));
        assert_eq!(
            build_tool(Some(WINDOWS_MSVC_TARGET)),
            if cfg!(windows) {
                BuildTool::Cargo
            } else {
                BuildTool::Xwin
            }
        );
        let args = cargo_build_args(
            "arena",
            Profile::Shipping,
            Some(WINDOWS_MSVC_TARGET),
            BuildTool::Xwin,
            &[],
        );
        assert_eq!(
            args,
            vec![
                "xwin".to_string(),
                "build".to_string(),
                "--profile".to_string(),
                "shipping".to_string(),
                "--target".to_string(),
                WINDOWS_MSVC_TARGET.to_string(),
                "-p".to_string(),
                "arena".to_string(),
            ]
        );
        let host = cargo_build_args("arena", Profile::Dev, None, BuildTool::Cargo, &[]);
        assert_eq!(
            host,
            vec![
                "build".to_string(),
                "--profile".to_string(),
                "dev".to_string(),
                "-p".to_string(),
                "arena".to_string(),
            ]
        );
    }

    /// C-26 (ADR-8835): `--container` sets the glibc floor to the profile's, `--glibc` must agree,
    /// `--steam` needs the sniper profile, and the flag is Linux-only and parsed by name.
    #[test]
    fn container_flag_sets_the_profile_glibc_floor() -> Result<()> {
        use package_container::{CONTAINER_GLIBC_CONFLICT, ContainerProfile};
        let code = |err: anyhow::Error| err.downcast_ref::<ContainerError>().map(|e| e.code);
        let sniper = parse(&["--platform", "linux", "--container", "steam-sniper"])?;
        assert_eq!(sniper.container, Some(ContainerProfile::SteamSniper));
        assert_eq!(linux_glibc_floor(&sniper)?, Some(STEAM_RUNTIME_GLIBC));
        let debian = parse(&[
            "--platform",
            "linux",
            "--profile",
            "shipping",
            "--container",
            "debian12",
        ])?;
        assert_eq!(linux_glibc_floor(&debian)?, GlibcVersion::parse("2.36"));
        let agree = parse(&[
            "--platform",
            "linux",
            "--container",
            "debian12",
            "--glibc",
            "2.36",
        ])?;
        assert_eq!(linux_glibc_floor(&agree)?, GlibcVersion::parse("2.36"));
        for argv in [
            &[
                "--platform",
                "linux",
                "--container",
                "debian12",
                "--glibc",
                "2.31",
            ][..],
            &[
                "--platform",
                "linux",
                "--container",
                "steam-sniper",
                "--glibc",
                "host",
            ][..],
            &["--platform", "linux", "--container", "debian12", "--steam"][..],
        ] {
            let err = linux_glibc_floor(&parse(argv)?).expect_err("conflict");
            assert_eq!(code(err), Some(CONTAINER_GLIBC_CONFLICT), "{argv:?}");
        }
        let steam = parse(&["--platform", "linux", "--container", "steam-sniper", "--steam"])?;
        assert_eq!(linux_glibc_floor(&steam)?, Some(STEAM_RUNTIME_GLIBC));
        // Unknown profiles and the image/mount flags without --container are usage errors.
        assert!(parse(&["--platform", "linux", "--container", "sniper"]).is_err());
        assert!(parse(&["--platform", "linux", "--container-image", "localhost/x:1"]).is_err());
        assert!(parse(&["--platform", "linux", "--container-mount", "/tmp"]).is_err());
        let mounts = parse(&[
            "--platform",
            "linux",
            "--container",
            "debian12",
            "--container-mount",
            "/a",
            "--container-mount",
            "/b",
            "--container-image",
            "localhost/regia-bullseye-rust:1.99",
        ])?;
        assert_eq!(
            mounts.container_mounts,
            [PathBuf::from("/a"), PathBuf::from("/b")]
        );
        assert_eq!(
            mounts.container_image.as_deref(),
            Some("localhost/regia-bullseye-rust:1.99")
        );
        Ok(())
    }

    /// `--container` on Windows or Android stops before any cook or build with its code.
    #[test]
    fn container_flag_is_linux_only() -> Result<()> {
        for platform in ["windows", "android"] {
            let args = parse(&["--platform", platform, "--container", "debian12"])?;
            let err = run(&args).expect_err("not Linux");
            assert_eq!(
                err.downcast_ref::<ContainerError>().map(|e| e.code),
                Some(package_container::CONTAINER_PLATFORM_UNSUPPORTED),
                "{platform}: {err:#}"
            );
        }
        Ok(())
    }

    /// The host side of a container build: the host's cargo call without `--target`, shipping paths
    /// remapped from the container's per-profile cargo home and target directory (not this machine's
    /// cargo home), the build identity passed in, and the profile directory cargo writes to.
    #[cfg(unix)]
    #[test]
    fn container_host_call_remaps_the_profile_dirs() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        if !root.join("samples/arena/Regia.toml").is_file() {
            eprintln!("skipped: this tree has no samples/arena (a source export)");
            return Ok(());
        }
        let shipping = build_profile::ResolvedProfile {
            name: "shipping".to_string(),
            cargo: Profile::Shipping,
            spec: None,
            server: false,
        };
        let profile = package_container::ContainerProfile::SteamSniper;
        let call = container_host_call(&root.join("samples/arena"), "arena", "arena", &shipping, profile)?;
        assert_eq!(call.workspace, root);
        assert_eq!(call.profile_dir, "shipping");
        assert_eq!(call.bin, "arena");
        assert_eq!(call.args[..5], ["build", "--profile", "shipping", "-p", "arena"]);
        assert!(!call.args.iter().any(|a| a == "--target"), "{:?}", call.args);
        let dirs = package_container::ProfileDirs::new(&call.target_dir, profile);
        let rustflags = call
            .args
            .iter()
            .find(|a| a.starts_with("target.") && a.contains("rustflags"))
            .expect("a shipping build remaps paths");
        let triple = format!("{}-unknown-linux-gnu", std::env::consts::ARCH);
        assert!(
            rustflags.starts_with(&format!("target.{triple}.rustflags=")),
            "{rustflags}"
        );
        for want in [
            format!("--remap-path-prefix={}=/cargo", dirs.cargo_home.display()),
            format!("--remap-path-prefix={}=/target", dirs.target.display()),
            format!("--remap-path-prefix={}=/regia", root.display()),
            "-Clink-arg=-Wl,-rpath,$ORIGIN".to_string(),
        ] {
            assert!(rustflags.contains(&want), "{want} missing from {rustflags}");
        }
        // The home remap lands on `/builder` (ADR-10354); a container build has no home remap.
        assert!(
            !rustflags.contains("=/home\"") && !rustflags.contains("=/builder\""),
            "the container's home is not this machine's: {rustflags}"
        );
        let identity: Vec<&str> = call.identity.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            identity,
            ["REGIA_GIT_HASH", "REGIA_BUILD_VERSION", "REGIA_BUILD_PROFILE"]
        );

        let dev = build_profile::ResolvedProfile {
            name: "dev".to_string(),
            cargo: Profile::Dev,
            spec: None,
            server: false,
        };
        let call = container_host_call(&root.join("samples/arena"), "arena", "arena", &dev, profile)?;
        assert_eq!(call.args, ["build", "--profile", "dev", "-p", "arena"]);
        assert_eq!(call.profile_dir, "debug");
        assert!(call.identity.is_empty());
        Ok(())
    }

    #[test]
    fn linux_glibc_floor_defaults_to_the_steam_runtime_for_shipping() -> Result<()> {
        let dev = parse(&["--platform", "linux"])?;
        assert_eq!(dev.glibc, None);
        assert_eq!(linux_glibc_floor(&dev)?, None, "dev packages link the host glibc");
        let shipping = parse(&["--platform", "linux", "--profile", "shipping"])?;
        assert_eq!(linux_glibc_floor(&shipping)?, Some(STEAM_RUNTIME_GLIBC));
        let host = parse(&["--platform", "linux", "--profile", "shipping", "--glibc", "host"])?;
        assert_eq!(linux_glibc_floor(&host)?, None);
        let older = parse(&["--platform", "linux", "--glibc", "2.28"])?;
        assert_eq!(linux_glibc_floor(&older)?, GlibcVersion::parse("2.28"));
        let bad = parse(&["--platform", "linux", "--glibc", "sniper"])?;
        assert!(linux_glibc_floor(&bad).is_err());
        // A Steam depot must run in the runtime: host glibc or a newer floor is refused.
        let steam_host = parse(&["--platform", "linux", "--steam", "--glibc", "host"])?;
        assert!(linux_glibc_floor(&steam_host).is_err());
        let steam_new = parse(&["--platform", "linux", "--steam", "--glibc", "2.35"])?;
        assert!(linux_glibc_floor(&steam_new).is_err());
        let steam_dev = parse(&["--platform", "linux", "--steam"])?;
        assert_eq!(
            linux_glibc_floor(&steam_dev)?,
            Some(STEAM_RUNTIME_GLIBC),
            "--steam implies the Steam runtime floor"
        );
        Ok(())
    }

    /// ADR-5489 lane: a Windows `regia package` stopped in the SBOM step, `cargo metadata --frozen`
    /// wanting `ab_glyph` (no Windows build uses it). The SBOM is filtered to the built triple.
    #[test]
    fn the_sbom_is_filtered_to_the_triple_the_package_builds() {
        assert_eq!(sbom_triples(Some(WINDOWS_MSVC_TARGET)), [WINDOWS_MSVC_TARGET]);
        assert_eq!(
            sbom_triples(Some("x86_64-unknown-linux-gnu.2.31")),
            ["x86_64-unknown-linux-gnu"],
            "the zigbuild glibc suffix is not part of the triple"
        );
        // A host build: this host's triple (every test host has rustc).
        let host = sbom_triples(None);
        assert_eq!(host.len(), 1, "{host:?}");
        assert!(host[0].contains(std::env::consts::ARCH), "{host:?}");
    }

    #[test]
    fn glibc_floor_builds_with_cargo_zigbuild() {
        let target = linux_glibc_target(STEAM_RUNTIME_GLIBC);
        assert_eq!(
            target,
            format!("{}-unknown-linux-gnu.2.31", std::env::consts::ARCH)
        );
        assert_eq!(build_tool(Some(&target)), BuildTool::Zigbuild);
        assert_eq!(build_tool(None), BuildTool::Cargo);
        assert_eq!(build_tool(Some("x86_64-unknown-linux-gnu")), BuildTool::Cargo);
        assert_eq!(
            cargo_target_dir_name("x86_64-unknown-linux-gnu.2.31"),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            cargo_target_dir_name("aarch64-unknown-linux-gnu.2.17"),
            "aarch64-unknown-linux-gnu"
        );
        assert_eq!(cargo_target_dir_name(WINDOWS_MSVC_TARGET), WINDOWS_MSVC_TARGET);
        let config = vec!["target.x86_64-unknown-linux-gnu.rustflags=[]".to_string()];
        let args = cargo_build_args(
            "arena",
            Profile::Shipping,
            Some("x86_64-unknown-linux-gnu.2.31"),
            BuildTool::Zigbuild,
            &config,
        );
        assert_eq!(
            args,
            [
                "zigbuild",
                "--profile",
                "shipping",
                "--target",
                "x86_64-unknown-linux-gnu.2.31",
                "-p",
                "arena",
                "--config",
                "target.x86_64-unknown-linux-gnu.rustflags=[]",
            ]
        );
    }

    #[test]
    fn shipping_remaps_build_machine_paths() -> Result<()> {
        let config = remap_config(
            "x86_64-unknown-linux-gnu",
            Path::new("/w/regia"),
            Path::new("/t/target"),
        );
        assert!(
            config.starts_with("target.x86_64-unknown-linux-gnu.rustflags=["),
            "{config}"
        );
        // The value is a TOML array cargo can parse; the most specific prefixes come last.
        let value: toml::Value =
            toml::from_str(&format!("v = {}", &config[config.find('[').unwrap_or(0)..]))?;
        let flags: Vec<&str> = value["v"]
            .as_array()
            .map(|a| a.iter().filter_map(toml::Value::as_str).collect())
            .unwrap_or_default();
        let n = flags.len();
        assert!(n >= 3, "{flags:?}");
        assert_eq!(flags[n - 3], "--remap-path-prefix=/w/regia=/regia");
        assert_eq!(flags[n - 2], "--remap-path-prefix=/t/target=/target");
        assert_eq!(
            flags[n - 1],
            "-Clink-arg=-Wl,-rpath,$ORIGIN",
            "Linux binaries find libs next to them"
        );
        // ADR-10354: no remap may land on a prefix the packaging host-string gate scans for
        // (a `/home` target would reproduce the leak it hides).
        assert!(
            flags.iter().all(|flag| {
                flag.rsplit_once('=')
                    .is_none_or(|(_, to)| !(to.starts_with("/home") || to.contains("/home/")))
            }),
            "{flags:?}"
        );
        assert!(flags.iter().any(|flag| flag.ends_with("=/builder")), "{flags:?}");
        // Windows paths keep their backslashes through the TOML string escape.
        let windows = remap_config(WINDOWS_MSVC_TARGET, Path::new(r"C:\w"), Path::new(r"C:\t"));
        let value: toml::Value =
            toml::from_str(&format!("v = {}", &windows[windows.find('[').unwrap_or(0)..]))?;
        let windows: Vec<&str> = value["v"]
            .as_array()
            .map(|a| a.iter().filter_map(toml::Value::as_str).collect())
            .unwrap_or_default();
        assert!(
            windows.contains(&r"--remap-path-prefix=C:\w=/regia"),
            "{windows:?}"
        );
        assert!(
            !windows.iter().any(|f| f.contains("rpath")),
            "no rpath on Windows"
        );
        Ok(())
    }

    /// One `build <label>` block of `package-prebuild.sh --print`: its `env` and `arg` lines.
    #[cfg(unix)]
    fn printed_build(text: &str, label: &str) -> (Vec<String>, Vec<String>) {
        let mut env = Vec::new();
        let mut args = Vec::new();
        let mut inside = false;
        for line in text.lines() {
            if let Some(name) = line.strip_prefix("build ") {
                inside = name == label;
                continue;
            }
            if !inside {
                continue;
            }
            if let Some(value) = line.strip_prefix("env ") {
                env.push(value.to_string());
            } else if let Some(value) = line.strip_prefix("arg ") {
                args.push(value.to_string());
            }
        }
        (env, args)
    }

    /// The gate's build step for `package_e2e` (`tools/gaterig/package-prebuild.sh`, ADR-5719) makes
    /// the cargo calls `regia package` makes: with any other flag or identity value cargo takes the
    /// package's build for a new one and rebuilds every crate inside the test, which is the 20-minute
    /// fat-LTO link the step exists to keep out of it. Both run here in this process's environment
    /// (home, cargo home, target dir, git revision), for the host's glibc and for the Steam floor.
    #[cfg(unix)]
    #[test]
    fn package_prebuild_script_issues_the_cli_builds() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        let script = root.join("tools/gaterig/package-prebuild.sh");
        if !script.is_file() || !root.join("samples/arena/Regia.toml").is_file() {
            eprintln!("skipped: this tree has no tools/gaterig or no samples/arena (a source export)");
            return Ok(());
        }
        let shipping = build_profile::ResolvedProfile {
            name: "shipping".to_string(),
            cargo: Profile::Shipping,
            spec: None,
            server: false,
        };
        let dev = build_profile::ResolvedProfile {
            name: "dev".to_string(),
            cargo: Profile::Dev,
            spec: None,
            server: false,
        };
        for glibc in ["host", "2.31"] {
            let printed = Command::new("bash")
                .arg(&script)
                .arg("--print")
                .env("REGIA_PACKAGE_E2E_GLIBC", glibc)
                .current_dir(&root)
                .output()?;
            let text = String::from_utf8_lossy(&printed.stdout).into_owned();
            assert!(
                printed.status.success(),
                "package-prebuild.sh --print failed: {text}{}",
                String::from_utf8_lossy(&printed.stderr)
            );

            // `package_e2e` passes `--glibc host` or the floor; the CLI maps the floor to cargo-zigbuild.
            let target = (glibc != "host").then(|| linux_glibc_target(STEAM_RUNTIME_GLIBC));
            let call = cargo_build_call(&root.join("samples/arena"), "arena", &shipping, target.as_deref())?;
            assert_eq!(call.workspace, root);
            let mut args = vec!["cargo".to_string()];
            args.extend(call.args);
            let env: Vec<String> = call
                .env
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            assert_eq!(
                printed_build(&text, "shipping"),
                (env, args),
                "the shipping build of package-prebuild.sh (glibc {glibc}) is not `regia package`'s"
            );

            // The dev build: no remaps, no identity; the test gives it a target dir of its own.
            let call = cargo_build_call(&root.join("samples/sim_bridge"), "sim_bridge", &dev, None)?;
            assert!(call.env.is_empty(), "{:?}", call.env);
            let mut args = vec!["cargo".to_string()];
            args.extend(call.args);
            let (env, printed_args) = printed_build(&text, "dev");
            assert_eq!(
                printed_args, args,
                "the dev build of package-prebuild.sh is not `regia package`'s"
            );
            // Where `package_e2e`'s `dev_target_dir` points the build: beside its own binary, in the
            // target dir cargo built the tests into (this binary's too), symlinks resolved. Not
            // CARGO_TARGET_DIR read back: a `build.target-dir` config or a symlinked path moves the
            // tests without moving that.
            let test_target = std::env::current_exe()?
                .canonicalize()?
                .ancestors()
                .nth(3)
                .map(Path::to_path_buf)
                .context("this test binary is under a target dir (<target>/debug/deps)")?;
            assert_eq!(
                env,
                [format!(
                    "CARGO_TARGET_DIR={}",
                    test_target.join("package-e2e").display()
                )],
                "the dev build goes to package_e2e's own target dir: package-prebuild.sh, run with this \
                 process's environment, builds sim_bridge elsewhere than beside this test binary (run the tests \
                 through cargo, or with the CARGO_TARGET_DIR they were built with)"
            );
        }
        Ok(())
    }

    /// `package-prebuild.sh` bakes in the `[project]` version the CLI parses from the same
    /// `Regia.toml`, whatever follows the string on its line (a comment holding quotes) and in
    /// either TOML string form: another value is another build to cargo, and the gate's package
    /// would rebuild every crate.
    #[cfg(unix)]
    #[test]
    fn package_prebuild_script_reads_the_version_the_cli_reads() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        let script = root.join("tools/gaterig/package-prebuild.sh");
        let arena = root.join("samples/arena/Regia.toml");
        if !script.is_file() || !arena.is_file() {
            eprintln!("skipped: this tree has no tools/gaterig or no samples/arena (a source export)");
            return Ok(());
        }
        let manifest = std::fs::read_to_string(&arena)?;
        let line = manifest
            .lines()
            .find(|line| line.starts_with("version"))
            .context("arena's Regia.toml has a version line")?;
        for (version_line, version) in [
            (r#"version = "0.1.0" # bump with "regia version""#, "0.1.0"),
            (r#"version="2.3.4""#, "2.3.4"),
            ("version = '1.0.0-rc.1' # \"quoted\"", "1.0.0-rc.1"),
            (r#"  version   =   "0.9.0"   "#, "0.9.0"),
        ] {
            // A workspace of its own: a Cargo.toml cargo can read, and arena's manifest with this
            // version line.
            let ws = regia_test_utils::unique_temp_dir("prebuild_version");
            std::fs::create_dir_all(ws.join("src"))?;
            std::fs::create_dir_all(ws.join("samples/arena"))?;
            std::fs::write(
                ws.join("Cargo.toml"),
                "[package]\nname = \"prebuild_version\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
            )?;
            std::fs::write(ws.join("src/lib.rs"), "")?;
            let project = ws.join("samples/arena");
            std::fs::write(
                project.join("Regia.toml"),
                manifest.replacen(line, version_line, 1),
            )?;
            assert_eq!(
                Manifest::load(&project)?.project.version,
                version,
                "the CLI's parse of {version_line:?}"
            );

            let printed = Command::new("bash")
                .arg(&script)
                .arg("--print")
                .env("REGIA_PACKAGE_E2E_GLIBC", "host")
                .env("REGIA_GIT_HASH", "prebuild-version")
                .current_dir(&ws)
                .output()?;
            let text = String::from_utf8_lossy(&printed.stdout).into_owned();
            assert!(
                printed.status.success(),
                "package-prebuild.sh --print failed on {version_line:?}: {text}{}",
                String::from_utf8_lossy(&printed.stderr)
            );
            let (env, _) = printed_build(&text, "shipping");
            assert!(
                env.contains(&format!("REGIA_BUILD_VERSION={version}")),
                "package-prebuild.sh read {version_line:?} as another version than the CLI's {version:?}: {env:?}"
            );
            let _ = std::fs::remove_dir_all(&ws);
        }
        Ok(())
    }

    #[test]
    fn args_parse_with_defaults() -> Result<()> {
        let args = parse(&["--platform", "android", "--format", "apk"])?;
        assert_eq!(args.platform, Platform::Android);
        assert_eq!(args.format, Some(Format::Apk));
        assert_eq!(args.profile_name, "dev");
        assert!(!args.dry_run, "dry run is opt-in");
        assert_eq!(args.project, PathBuf::from("."));
        assert_eq!(args.out, None);
        assert!(!args.steam, "steam is opt-in");
        let args = parse(&[
            "--platform",
            "android",
            "--profile",
            "shipping",
            "--project",
            "/p",
            "--out",
            "/o",
        ])?;
        assert_eq!(args.profile_name, "shipping");
        assert_eq!(
            (args.project, args.out),
            (PathBuf::from("/p"), Some(PathBuf::from("/o")))
        );
        let args = parse(&["--platform", "linux", "--profile", "demo", "--dry-run"])?;
        assert_eq!((args.profile_name.as_str(), args.dry_run), ("demo", true));
        assert!(parse(&["--platform", "macos"]).is_err());
        assert!(parse(&["--format", "apk"]).is_err());
        assert!(parse(&["--platform", "linux", "--steam"]).is_ok());
        Ok(())
    }

    fn error_of(argv: &[&str]) -> String {
        match parse(argv).and_then(|args| run(&args)) {
            Ok(_) => String::new(),
            Err(e) => format!("{e:#}"),
        }
    }

    /// Creates a temp directory with a minimal `Regia.toml` so format-validation
    /// tests can reach the per-platform checks without failing on manifest loading.
    fn temp_project() -> PathBuf {
        // Unique per call: tests run on parallel threads of one process and must not share a dir.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("regia_cli_test_proj_{pid}_{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let manifest = r#"
[project]
name = "Test Project"
id = "com.test.proj"
"#;
        std::fs::write(dir.join("Regia.toml"), manifest).expect("write Regia.toml");
        dir
    }

    #[test]
    fn linux_and_windows_not_unimplemented() {
        let dir = temp_project();
        // These should give a clear error about missing files, not "M16 not implemented".
        let linux_err = error_of(&["--platform", "linux", "--project", &dir.to_string_lossy()]);
        assert!(
            !linux_err.is_empty() && !linux_err.contains("M16"),
            "linux packaging must fail with a real reason, not M16 or success: {linux_err}"
        );
        let windows_err = error_of(&["--platform", "windows", "--project", &dir.to_string_lossy()]);
        assert!(
            !windows_err.is_empty() && !windows_err.contains("M16"),
            "windows packaging must fail with a real reason, not M16 or success: {windows_err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_project_fails_before_building() {
        let message = error_of(&["--platform", "android", "--project", "/nonexistent/regia/project"]);
        assert!(message.contains("not found"), "{message}");
    }

    #[test]
    fn steam_is_desktop_only() {
        let dir = temp_project();
        let message = error_of(&[
            "--platform",
            "android",
            "--steam",
            "--project",
            &dir.to_string_lossy(),
        ]);
        assert!(message.contains("--steam is a desktop option"), "{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_format_for_platform_errors() {
        let dir = temp_project();
        assert!(
            error_of(&[
                "--platform",
                "android",
                "--format",
                "zip",
                "--project",
                &dir.to_string_lossy()
            ])
            .contains("not an Android format")
        );
        assert!(
            error_of(&[
                "--platform",
                "linux",
                "--format",
                "apk",
                "--project",
                &dir.to_string_lossy()
            ])
            .contains("not supported for Linux")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn abi_filter_narrows_the_manifest() -> Result<()> {
        let manifest = Manifest::parse(
            "[project]\nname = \"x\"\nid = \"a.b\"\n[platforms.android]\nabis = [\"arm64-v8a\", \"x86_64\"]\n",
        )?;
        let all = with_android_abis(&manifest, &[])?;
        assert_eq!(all.platforms.android.abis, ["arm64-v8a", "x86_64"]);
        let arm = with_android_abis(&manifest, &["arm64-v8a".to_owned()])?;
        assert_eq!(arm.platforms.android.abis, ["arm64-v8a"]);
        let args = cargo_ndk_args(&arm.platforms.android, Path::new("/j"), Profile::Dev);
        assert!(!args.iter().any(|a| a == "x86_64"));
        let err = with_android_abis(&manifest, &["armeabi-v7a".to_owned()])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("not in [platforms.android] abis"), "{err}");
        Ok(())
    }

    /// A solid `size`x`size` PNG of `rgba` at `path`.
    fn write_png(path: &Path, size: u32, rgba: [u8; 4]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("icon dir");
        }
        image::RgbaImage::from_pixel(size, size, image::Rgba(rgba))
            .save_with_format(path, image::ImageFormat::Png)
            .expect("write png");
    }

    /// The size of `res/<bucket>/<name>.png` and its centre pixel snapped to `expected` when every
    /// channel is within 1 (resampling rounding), so a wrong source still shows up as a mismatch.
    fn mipmap(res: &Path, bucket: &str, name: &str, expected: [u8; 4]) -> (u32, [u8; 4]) {
        let icon = image::open(res.join(bucket).join(format!("{name}.png")))
            .unwrap_or_else(|e| panic!("{bucket}/{name}.png: {e}"))
            .to_rgba8();
        assert_eq!(icon.width(), icon.height(), "{bucket}/{name}.png is square");
        let centre = icon.get_pixel(icon.width() / 2, icon.height() / 2).0;
        let close = centre
            .iter()
            .zip(expected)
            .all(|(got, want)| got.abs_diff(want) <= 1);
        (icon.width(), if close { expected } else { centre })
    }

    /// Specs/21 § 2 and § 4 Android step 2 (ADR-5627): `[platforms.android] icon` and
    /// `adaptive_icon_foreground` come first, the conventional paths follow, and the CLI writes
    /// every mipmap density from the PNG (48 dp launcher, 108 dp foreground). Before the fix the
    /// keys were ignored, only `mipmap-mdpi` got a copy, and the manifest never named the icon.
    #[test]
    fn android_icons_follow_the_manifest_keys_and_fill_every_density() {
        const CONFIGURED: [u8; 4] = [200, 10, 10, 255];
        const CONFIGURED_FG: [u8; 4] = [10, 200, 10, 255];
        const DEFAULT: [u8; 4] = [10, 10, 200, 255];
        let dir = regia_test_utils::unique_temp_dir("pkg_android_icons");
        write_png(&dir.join("art/launcher.png"), 512, CONFIGURED);
        write_png(&dir.join("art/fg.png"), 432, CONFIGURED_FG);
        write_png(&dir.join("assets/branding/icon.png"), 256, DEFAULT);
        write_png(&dir.join("assets/branding/icon_fg.png"), 256, DEFAULT);
        let manifest = Manifest::parse(
            "[project]\nname = \"G\"\nid = \"com.example.g\"\n\n[platforms.android]\n\
             icon = \"art/launcher.png\"\nadaptive_icon_foreground = \"art/fg.png\"\n",
        )
        .expect("manifest");
        let android = &manifest.platforms.android;
        assert_eq!(android.icon.as_deref(), Some("art/launcher.png"));
        let gradle = dir.join("gradle");
        copy_android_icons(&dir, &gradle, android).expect("icons");
        let res = gradle.join("app/src/main/res");
        for (bucket, launcher, foreground) in MIPMAP_DENSITIES {
            assert_eq!(
                mipmap(&res, bucket, "ic_launcher", CONFIGURED),
                (launcher, CONFIGURED),
                "{bucket}"
            );
            assert_eq!(
                mipmap(&res, bucket, "ic_launcher_foreground", CONFIGURED_FG),
                (foreground, CONFIGURED_FG),
                "{bucket}"
            );
        }
        assert_eq!(mipmap(&res, "mipmap-xxxhdpi", "ic_launcher", CONFIGURED).0, 192);
        assert!(res.join("mipmap-anydpi-v26/ic_launcher.xml").is_file());

        // Unset keys (and a key naming a missing file) fall back to the conventional paths.
        let gradle_default = dir.join("gradle-default");
        let missing = AndroidPlatform {
            icon: Some("art/nope.png".into()),
            ..Default::default()
        };
        copy_android_icons(&dir, &gradle_default, &missing).expect("icons");
        let res = gradle_default.join("app/src/main/res");
        assert_eq!(mipmap(&res, "mipmap-hdpi", "ic_launcher", DEFAULT), (72, DEFAULT));
        assert_eq!(
            mipmap(&res, "mipmap-hdpi", "ic_launcher_foreground", DEFAULT),
            (162, DEFAULT)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A project without any icon still gets `ic_launcher` in every density (a placeholder), so
    /// the manifest's `@mipmap/ic_launcher` resolves, and no adaptive icon; a file that is not an
    /// image fails the package with its path in the message.
    #[test]
    fn android_icons_fall_back_to_a_placeholder_and_refuse_a_non_image() {
        let dir = regia_test_utils::unique_temp_dir("pkg_android_icon_placeholder");
        std::fs::create_dir_all(&dir).expect("dir");
        let gradle = dir.join("gradle");
        // A stale adaptive icon from an earlier package must not survive.
        let adaptive = gradle.join("app/src/main/res/mipmap-anydpi-v26/ic_launcher.xml");
        std::fs::create_dir_all(adaptive.parent().expect("parent")).expect("anydpi");
        std::fs::write(&adaptive, "<adaptive-icon/>").expect("stale xml");
        copy_android_icons(&dir, &gradle, &AndroidPlatform::default()).expect("placeholder");
        let res = gradle.join("app/src/main/res");
        for (bucket, launcher, _) in MIPMAP_DENSITIES {
            assert_eq!(
                mipmap(&res, bucket, "ic_launcher", [0; 4]).0,
                launcher,
                "{bucket}"
            );
        }
        assert!(!adaptive.exists(), "no foreground, no adaptive icon");

        std::fs::create_dir_all(dir.join("assets/branding")).expect("branding");
        std::fs::write(dir.join("assets/branding/icon.png"), b"not a png").expect("junk");
        let error = copy_android_icons(&dir, &gradle, &AndroidPlatform::default())
            .expect_err("a junk icon must fail the package");
        assert!(format!("{error:#}").contains("icon.png"), "{error:#}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ADR-5627: a workspace without `[profile.shipping]` (a project generated before the templates
    /// carried it) gets the spec's shipping profile through `--config`; one that defines the table
    /// keeps its own. The values are the ones the engine workspace's `Cargo.toml` uses.
    #[test]
    fn a_missing_shipping_profile_is_supplied_through_config() {
        let dir = regia_test_utils::unique_temp_dir("pkg_shipping_profile");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = [\"game\"]\n").expect("manifest");
        let config = shipping_profile_config(&dir);
        assert_eq!(config.len(), SHIPPING_PROFILE.len(), "{config:?}");
        assert!(
            config.contains(&"profile.shipping.inherits=\"release\"".to_string()),
            "{config:?}"
        );
        assert!(
            config.contains(&"profile.shipping.strip=\"none\"".to_string()),
            "{config:?}"
        );
        // Every value parses as the TOML cargo reads, and matches the engine workspace's profile.
        let engine: toml::Table = toml::from_str(include_str!("../../../../Cargo.toml")).expect("workspace");
        let engine = engine["profile"]["shipping"]
            .as_table()
            .expect("[profile.shipping]")
            .clone();
        let mut supplied = toml::Table::new();
        for value in &config {
            let (key, value) = value
                .strip_prefix("profile.shipping.")
                .and_then(|rest| rest.split_once('='))
                .expect("profile.shipping.<key>=<value>");
            let parsed: toml::Table = toml::from_str(&format!("v = {value}")).expect("TOML value");
            supplied.insert(key.to_string(), parsed["v"].clone());
        }
        assert_eq!(supplied, engine);

        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = []\n\n[profile.shipping]\ninherits = \"release\"\npanic = \"unwind\"\n",
        )
        .expect("manifest with a profile");
        assert!(
            shipping_profile_config(&dir).is_empty(),
            "a project's own profile wins"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cargo_ndk_command_line() -> Result<()> {
        let mut android = Manifest::parse("[project]\nname = \"x\"\nid = \"a.b\"\n")?
            .platforms
            .android;
        android.abis.push("x86_64".into());
        let args = cargo_ndk_args(
            &android,
            Path::new("/p/build/android/app/src/main/jniLibs"),
            Profile::Dev,
        );
        let text: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            text,
            [
                "ndk",
                "-t",
                "arm64-v8a",
                "-t",
                "x86_64",
                "-P",
                "29",
                "-o",
                "/p/build/android/app/src/main/jniLibs",
                "build",
                "-p",
                "android",
            ]
        );
        let shipping = cargo_ndk_args(&android, Path::new("/j"), Profile::Shipping);
        let text: Vec<String> = shipping
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(text.windows(2).any(|w| w == ["--profile", "shipping"]));
        Ok(())
    }

    #[test]
    fn output_paths() {
        let project = Path::new("/games/hello");
        assert_eq!(gradle_dir(project), PathBuf::from("/games/hello/build/android"));
        assert_eq!(
            default_out_dir(project, "android"),
            PathBuf::from("/games/hello/dist/android")
        );
        assert_eq!(
            default_out_dir(project, "linux"),
            PathBuf::from("/games/hello/dist/linux")
        );
        assert_eq!(debug_apk_name("com.example.hello"), "com.example.hello-debug.apk");
        // The packager writes `{slug}-debug.apk`; the lookup must slug the same way.
        assert_eq!(debug_apk_name("com.Example.Game"), "com.example.game-debug.apk");
    }

    #[test]
    fn workspace_root_of_this_repo() -> Result<()> {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = cargo_workspace_root(here)?;
        assert!(
            root.join("crates").join("regia_cli").is_dir(),
            "{}",
            root.display()
        );
        Ok(())
    }

    #[test]
    fn assets_are_copied_into_the_apk_folder() -> Result<()> {
        let base = std::env::temp_dir().join(format!("regia_cli_test_{}_assets", std::process::id()));
        let project = base.join("project");
        let gradle = base.join("build/android");
        std::fs::create_dir_all(project.join("assets/config"))?;
        std::fs::create_dir_all(&gradle)?;
        std::fs::write(project.join("assets/about.txt"), "about")?;
        std::fs::write(project.join("assets/config/touch.ron"), "sensitivity = 0.003")?;
        std::fs::write(
            project.join("assets/.regia-scratch"),
            "ignored? no: copied verbatim",
        )?;

        let copied = copy_dir(&project.join("assets"), &gradle.join(APK_ASSETS_DIR))?;
        assert_eq!(copied, 3, "every file under assets/ is packaged");
        for packaged in [
            gradle.join(APK_ASSETS_DIR).join("about.txt"),
            gradle.join(APK_ASSETS_DIR).join("config/touch.ron"),
        ] {
            assert!(packaged.is_file(), "{} missing", packaged.display());
        }

        // A second run overwrites instead of duplicating.
        assert_eq!(
            copy_dir(&project.join("assets"), &gradle.join(APK_ASSETS_DIR))?,
            3
        );

        std::fs::remove_dir_all(&base)?;
        Ok(())
    }

    #[test]
    fn project_without_assets_packages_nothing() -> Result<()> {
        let base = std::env::temp_dir().join(format!("regia_cli_test_{}_noassets", std::process::id()));
        let project = base.join("project");
        std::fs::create_dir_all(&project)?;
        let gradle = base.join("build/android");
        std::fs::create_dir_all(&gradle)?;

        // copy_dir on a non-existent directory should fail, not create empty dir
        let assets = project.join("assets");
        assert!(copy_dir(&assets, &gradle.join(APK_ASSETS_DIR)).is_err());

        std::fs::remove_dir_all(&base)?;
        Ok(())
    }

    #[test]
    fn build_info_json_structure() {
        let manifest = Manifest::parse("[project]\nname = \"T\"\nid = \"a.b\"\n").expect("parse");
        let tmp = std::env::temp_dir().join(format!("regia_cli_test_buildinfo_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let mut extra = serde_json::Map::new();
        extra.insert("glibc_floor".into(), "2.31".into());
        // The revision comes from the project's tree (this repo), not the staging directory.
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        write_build_info(&tmp, &project, &manifest, extra).expect("write");
        let info_bytes = std::fs::read_to_string(tmp.join("build_info.json")).expect("read");
        let info: serde_json::Value = serde_json::from_str(&info_bytes).expect("json");
        assert_eq!(info["glibc_floor"], "2.31");
        if process::try_output(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&project),
        )
        .is_some()
        {
            assert_ne!(
                info["git"], "unknown",
                "build_info git must come from the source tree"
            );
        }
        assert_eq!(info["version"], "0.1.0");
        assert_eq!(info["build_number"], 1);
        assert_eq!(info["engine"], env!("CARGO_PKG_VERSION"));
        assert!(info["date"].as_str().is_some_and(|d| d.len() == 10));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn shipping_symbols_are_kept_next_to_the_package() {
        let base = std::env::temp_dir().join(format!("regia_cli_test_symbols_{}", std::process::id()));
        let out = base.join("out");
        std::fs::create_dir_all(&out).expect("mkdir");
        let binary = base.join("game");
        std::fs::write(&binary, "binary").expect("write binary");

        // Dev builds keep symbols in the binary; nothing is extracted.
        keep_shipping_symbols(&binary, &binary, &out, Profile::Dev, SymbolFormat::Dwarf).expect("dev no-op");
        assert!(!out.join("symbols").exists(), "dev builds produce no symbols dir");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn missing_shipping_symbols_fail_the_package() {
        let base =
            std::env::temp_dir().join(format!("regia_cli_test_symbols_missing_{}", std::process::id()));
        let out = base.join("out");
        std::fs::create_dir_all(&out).expect("mkdir");

        // A shipping build with no .pdb on disk must fail (not warn).
        let binary = base.join("game");
        std::fs::write(&binary, "binary").expect("write binary");
        let err = keep_shipping_symbols(&binary, &binary, &out, Profile::Shipping, SymbolFormat::Pdb);
        assert!(err.is_err(), "a missing .pdb fails the package: {err:?}");

        std::fs::remove_dir_all(&base).ok();
    }

    /// The Windows PDB is stored under its own file name (`symbols/<name>.pdb`), not the exe name
    /// (`symbols/<name>.exe`), as specs/21 § 4.4 and the packaging guide promise.
    #[test]
    fn windows_pdb_is_stored_under_its_own_file_name() {
        let base = std::env::temp_dir().join(format!("regia_cli_test_symbols_pdb_{}", std::process::id()));
        let out = base.join("out");
        std::fs::create_dir_all(&out).expect("mkdir");

        // The MSVC linker writes `game.pdb` next to `game.exe`.
        let binary = base.join("game.exe");
        std::fs::write(&binary, "MZ not really an exe").expect("write exe");
        let pdb = base.join("game.pdb");
        std::fs::write(&pdb, "not really a pdb").expect("write pdb");
        let staged = base.join("staged_game.exe");
        std::fs::copy(&binary, &staged).expect("stage");

        keep_shipping_symbols(&binary, &staged, &out, Profile::Shipping, SymbolFormat::Pdb)
            .expect("keep symbols");

        // Destination is the .pdb file name, not the exe name.
        let dest = out.join("symbols").join("game.pdb");
        assert!(dest.is_file(), "PDB stored at {}", dest.display());
        assert!(
            !out.join("symbols").join("game.exe").exists(),
            "PDB must not be stored under the exe name"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn hyphenated_exe_finds_the_underscored_pdb() -> Result<()> {
        // Cargo writes `my_game.pdb` (crate name) next to `my-game.exe` (binary name).
        let base = regia_test_utils::unique_temp_dir("cli-pdb-hyphen");
        let out = base.join("out");
        std::fs::create_dir_all(&out)?;
        let binary = base.join("my-game.exe");
        std::fs::write(&binary, "MZ not really an exe")?;
        std::fs::write(base.join("my_game.pdb"), "not really a pdb")?;
        let staged = base.join("staged.exe");
        std::fs::copy(&binary, &staged)?;
        assert_eq!(
            pdb_candidates(&binary),
            [base.join("my_game.pdb"), base.join("my-game.pdb")]
        );
        assert_eq!(pdb_candidates(&base.join("game.exe")), [base.join("game.pdb")]);
        keep_shipping_symbols(&binary, &staged, &out, Profile::Shipping, SymbolFormat::Pdb)?;
        assert!(out.join("symbols").join("my_game.pdb").is_file());
        Ok(())
    }

    /// The symbol artifact must resolve an address to a function name (not just be any copied
    /// bytes). Skips when the toolchain (`objcopy`, `addr2line`, `nm`, `rustc`) is unavailable.
    #[test]
    fn shipping_symbols_resolve_a_function_name() {
        for tool in ["objcopy", "addr2line", "nm", "rustc"] {
            if process::try_output(Command::new(tool).arg("--version")).is_none() {
                eprintln!("skipping shipping_symbols_resolve_a_function_name: `{tool}` unavailable");
                return;
            }
        }

        let base = std::env::temp_dir().join(format!("regia_cli_symbols_resolve_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir");
        let out = base.join("out");
        std::fs::create_dir_all(&out).expect("mkdir out");

        // A tiny program with a uniquely-named function, built with line tables and an unstripped
        // symbol table — exactly what `[profile.shipping]` produces now.
        let src = base.join("probe.rs");
        std::fs::write(
            &src,
            "fn regia_probe_marker() -> u32 { 42 }\nfn main() { std::hint::black_box(regia_probe_marker()); }\n",
        )
        .expect("write probe");
        let binary = base.join("probe");
        process::run(
            Command::new("rustc")
                .arg("--edition=2024")
                .arg("-C")
                .arg("debuginfo=line-tables-only")
                .arg("-C")
                .arg("strip=none")
                .arg("-o")
                .arg(&binary)
                .arg(&src),
            "install rustc",
        )
        .expect("compile probe");

        let staged = base.join("staged_probe");
        std::fs::copy(&binary, &staged).expect("stage");

        keep_shipping_symbols(&binary, &staged, &out, Profile::Shipping, SymbolFormat::Dwarf)
            .expect("keep symbols");
        let debug = out.join("symbols").join("probe.debug");
        assert!(debug.is_file(), "symbol artifact at {}", debug.display());

        // Find the function's address and resolve it through the artifact.
        let nm = process::try_output(Command::new("nm").arg(&binary)).expect("nm runs");
        let addr = nm
            .lines()
            .find(|l| l.contains("regia_probe_marker"))
            .and_then(|l| l.split_whitespace().next())
            .unwrap_or_else(|| panic!("no regia_probe_marker symbol in nm output:\n{nm}"));
        let resolved = process::try_output(
            Command::new("addr2line")
                .args(["-C", "-f", "-e"])
                .arg(&debug)
                .arg(addr),
        )
        .expect("addr2line runs");
        assert!(
            resolved.contains("regia_probe_marker"),
            "the symbol artifact resolves the function name:\n{resolved}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cookable_source_detection() {
        for cookable in [
            "meshes/hero.glb",
            "meshes/hero.gltf",
            "textures/rock.png",
            "textures/rock.JPG",
            "materials/steel.material.ron",
            "scatter/rocks.scatter.ron",
            "tables/falloff.curves.ron",
            "tables/xp.curves.csv",
            "anim/hit.chooser.ron",
            "props/chair.smartobject.ron",
            "camera/player.camera.ron",
            "tables/weapons.table.csv",
            "data/weapons.table.ron",
            "levels/intro.procgen/0000000000000000000000000000000a.pgout",
        ] {
            assert!(is_cookable_source(Path::new(cookable)));
        }
        for raw in [
            "about.txt",
            "README.md",
            "config/touch.ron",
            "levels/intro.level.ron",
            "textures/terrain.layers.ron",
            "fonts/ui.ttf",
            "meshes/hero.glb.meta",
        ] {
            assert!(!is_cookable_source(Path::new(raw)));
        }
        assert!(ships_raw_too(Path::new("props/chair.smartobject.ron")));
        assert!(ships_raw_too(Path::new("camera/player.camera.ron")));
        // Typed data tables keep shipping raw; the pak holds their cooked form too (ADR-2132).
        assert!(ships_raw_too(Path::new("tables/weapons.table.csv")));
        assert!(ships_raw_too(Path::new("data/Weapons.TABLE.ron")));
        assert!(!ships_raw_too(Path::new("tables/falloff.curves.ron")));
        assert!(!ships_raw_too(Path::new("anim/hit.chooser.ron")));
        // The ability formats cook into the pak and ship cooked only (ADR-9850 (c), owner decision
        // 2026-11-12, replacing ADR-9370's raw copy; integ-v06h, rev-v06-editor-ext finding 4).
        for ability in [
            "stats/hero.attrs.ron",
            "effects/poison.effect.ron",
            "abilities/bolt.ability.ron",
        ] {
            assert!(is_cookable_source(Path::new(ability)), "{ability}");
            assert!(!ships_raw_too(Path::new(ability)), "{ability}");
            assert!(is_raw_ability_source(Path::new(ability)), "{ability}");
        }
    }

    #[test]
    fn empty_or_corrupt_pak_is_not_fresh() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("regia_cli_pakcheck_{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;

        // A pak that does not exist is not fresh.
        assert!(!base_pak_is_fresh(&dir.join("missing.fpak"), &dir));

        // An empty file is not a valid FPAK archive.
        let empty = dir.join("empty.fpak");
        std::fs::write(&empty, b"")?;
        assert!(!base_pak_is_fresh(&empty, &dir));

        // Garbage bytes are not a valid FPAK archive.
        let garbage = dir.join("garbage.fpak");
        std::fs::write(&garbage, b"this is not a pak")?;
        assert!(!base_pak_is_fresh(&garbage, &dir));

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn packaged_media_ships_an_index_with_stamps() -> Result<()> {
        let base = regia_test_utils::unique_temp_dir("package-media-index");
        let project = base.join("project");
        // Nested media, a top-level clip, a non-media file: only the media is indexed, and the
        // paths are relative to the assets root (the APK layout the runtime extracts).
        std::fs::create_dir_all(project.join("assets/movies/deep"))?;
        std::fs::write(project.join("assets/movies/intro.webm"), b"\x1a\x45\xdf\xa3intro")?;
        std::fs::write(project.join("assets/movies/deep/cut.mkv"), b"\x1a\x45\xdf\xa3cut")?;
        std::fs::write(
            project.join("assets/movies/intro.video.ron"),
            "(format_version: 1, file: \"intro.webm\")",
        )?;
        std::fs::write(project.join("assets/about.txt"), "hello")?;
        let dest = base.join("package/assets");
        copy_raw_assets(&project, &dest)?;

        let index: regia::PackagedMediaIndex =
            ron::from_str(&std::fs::read_to_string(dest.join(regia::PACKAGED_MEDIA_INDEX))?)?;
        assert_eq!(index.format_version, 1);
        let entries: Vec<(String, u64, u64)> = index
            .files
            .iter()
            .map(|e| (e.path.clone(), e.len, e.hash))
            .collect();
        let intro = std::fs::read(project.join("assets/movies/intro.webm"))?;
        assert_eq!(
            entries,
            [
                (
                    "movies/deep/cut.mkv".to_string(),
                    7,
                    regia::packaged_media_stamp(b"\x1a\x45\xdf\xa3cut")
                ),
                (
                    "movies/intro.video.ron".to_string(),
                    39,
                    regia::packaged_media_stamp(b"(format_version: 1, file: \"intro.webm\")")
                ),
                (
                    "movies/intro.webm".to_string(),
                    intro.len() as u64,
                    regia::packaged_media_stamp(&intro)
                ),
            ],
            "sorted, stamped, assets-root-relative: {entries:?}"
        );

        // A project without media ships no index at all.
        let bare = base.join("bare");
        std::fs::create_dir_all(bare.join("assets"))?;
        std::fs::write(bare.join("assets/about.txt"), "hi")?;
        let bare_dest = base.join("bare-package/assets");
        copy_raw_assets(&bare, &bare_dest)?;
        assert!(!bare_dest.join(regia::PACKAGED_MEDIA_INDEX).exists());
        Ok(())
    }

    /// An Android package always carries the media index, empty when the project ships no media.
    /// A launch prunes the extracted media its index does not list, and takes a package with no
    /// index for one built before the index existed, which it never prunes: without the empty
    /// index, a version that drops its last clip leaves every earlier clip in the app's storage,
    /// still opening by its old path. v047's packager wrote its index for an empty tree; the
    /// writer integ-v048 kept skips it. Desktop packages have no extraction and get no empty
    /// index.
    #[test]
    fn android_staging_writes_an_empty_media_index_for_a_project_without_media() -> Result<()> {
        let base = regia_test_utils::unique_temp_dir("package-media-index-empty");
        let project = base.join("project");
        std::fs::create_dir_all(project.join("assets"))?;
        std::fs::write(project.join("assets/note.txt"), b"note")?;

        let linux = base.join("linux/assets");
        let on_linux = stage_raw_assets(&project, &linux, Profile::Dev, "linux")?;
        assert!(
            !linux.join(regia::PACKAGED_MEDIA_INDEX).exists(),
            "desktop: no media, no index"
        );

        let android = base.join("android/assets");
        let staged = stage_raw_assets(&project, &android, Profile::Dev, "android")?;
        assert_eq!(staged, on_linux + 1, "the index is one more staged file");
        assert!(android.join("note.txt").is_file());
        let index: regia::PackagedMediaIndex = ron::from_str(&std::fs::read_to_string(
            android.join(regia::PACKAGED_MEDIA_INDEX),
        )?)?;
        assert_eq!(index.format_version, 1);
        assert!(index.files.is_empty(), "{:?}", index.files);

        // A project with no `assets/` directory at all is the same case.
        let bare = base.join("bare");
        std::fs::create_dir_all(&bare)?;
        let bare_android = base.join("bare-android/assets");
        assert_eq!(
            stage_raw_assets(&bare, &bare_android, Profile::Dev, "android")?,
            1
        );
        let index: regia::PackagedMediaIndex = ron::from_str(&std::fs::read_to_string(
            bare_android.join(regia::PACKAGED_MEDIA_INDEX),
        )?)?;
        assert!(index.files.is_empty());
        let bare_linux = base.join("bare-linux/assets");
        assert_eq!(stage_raw_assets(&bare, &bare_linux, Profile::Dev, "linux")?, 0);
        assert!(!bare_linux.exists(), "desktop: nothing staged, no directory");

        // With media the index lists it, as before.
        std::fs::create_dir_all(project.join("assets/movies"))?;
        std::fs::write(project.join("assets/movies/intro.webm"), b"\x1a\x45\xdf\xa3intro")?;
        let android_media = base.join("android-media/assets");
        stage_raw_assets(&project, &android_media, Profile::Dev, "android")?;
        let index: regia::PackagedMediaIndex = ron::from_str(&std::fs::read_to_string(
            android_media.join(regia::PACKAGED_MEDIA_INDEX),
        )?)?;
        let paths: Vec<&str> = index.files.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["movies/intro.webm"]);
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    /// The index lists what the package holds and the runtime extracts. A symlinked clip is not
    /// staged (a link must not pull bytes from outside `assets/`), so it is not indexed and its
    /// target is not read. A clip whose path the runtime's rule refuses (a `:` in a name) is left
    /// out too. v047's index writer walked the staged files and skipped links; the writer that
    /// replaced it in integ-v048 read through them.
    #[cfg(unix)]
    #[test]
    fn the_media_index_leaves_out_links_and_paths_the_runtime_refuses() -> Result<()> {
        let base = regia_test_utils::unique_temp_dir("package-media-index-links");
        let project = base.join("project");
        std::fs::create_dir_all(project.join("assets/movies"))?;
        std::fs::write(project.join("assets/movies/intro.webm"), b"\x1a\x45\xdf\xa3intro")?;
        std::fs::write(project.join("assets/movies/a:b.webm"), b"\x1a\x45\xdf\xa3colon")?;
        let outside = base.join("outside.webm");
        std::fs::write(&outside, b"\x1a\x45\xdf\xa3outside")?;
        std::os::unix::fs::symlink(&outside, project.join("assets/movies/link.webm"))?;
        let dest = base.join("package/assets");
        copy_raw_assets(&project, &dest)?;

        let index: regia::PackagedMediaIndex =
            ron::from_str(&std::fs::read_to_string(dest.join(regia::PACKAGED_MEDIA_INDEX))?)?;
        let paths: Vec<&str> = index.files.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["movies/intro.webm"],
            "only the staged, extractable clip is listed"
        );
        assert!(!dest.join("movies/link.webm").exists(), "the link is not staged");
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    #[test]
    fn a_localized_project_ships_bundles_for_its_shipping_locales_only() -> Result<()> {
        let base = regia_test_utils::unique_temp_dir("package-loc-bundles");
        let project = base.join("project");
        let loc = project.join("assets/localization");
        for (tag, text) in [
            ("en", "hello = Hello\nbye = Bye\n"),
            ("fr", "# TODO translate\nbye = Bye\nhello = Bonjour\n"),
            ("de", "hello = Hallo\nbye = Tschuess\n"),
        ] {
            std::fs::create_dir_all(loc.join(tag))?;
            std::fs::write(loc.join(tag).join("main.ftl"), text)?;
        }
        std::fs::write(
            loc.join("localization.ron"),
            "(format_version: 1, source: \"en\", locales: [\"fr\"], fallbacks: {})",
        )?;
        let dest = base.join("package/assets");
        copy_raw_assets(&project, &dest)?;
        assert!(dest.join("loc/index.ron").is_file());
        let listing: Vec<String> = std::fs::read_dir(dest.join("loc"))?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            dest.join("loc/fr.ftl").is_file() && dest.join("loc/en.ftl").is_file(),
            "{listing:?} {:?}",
            std::fs::read_to_string(dest.join("loc/index.ron"))
        );
        assert!(!dest.join("loc/de.ftl").exists(), "an unlisted locale is absent");
        assert!(
            !dest.join("localization/fr/main.ftl").exists(),
            "authoring files stay out"
        );
        let index = regia_ui::loc_bundle::BundleIndex::from_ron(&std::fs::read_to_string(
            dest.join("loc/index.ron"),
        )?)
        .map_err(anyhow::Error::msg)?;
        let tags: Vec<&str> = index.locales.iter().map(|l| l.tag.as_str()).collect();
        assert_eq!(tags, ["en", "fr"]);
        // The packaged layout loads as bundles; the placeholder key shows the source text.
        let mut l = regia_ui::Localization::load_bundles(&index, |file| {
            std::fs::read(dest.join(file)).map_err(|e| e.to_string())
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert!(l.set_locale("fr"));
        assert_eq!(l.resolve("hello", &[]), "Bonjour");
        assert_eq!(l.resolve("bye", &[]), "Bye");
        // Packaging twice gives the same bytes.
        let first = std::fs::read(dest.join("loc/fr.ftl"))?;
        copy_raw_assets(&project, &base.join("package2/assets"))?;
        assert_eq!(std::fs::read(base.join("package2/assets/loc/fr.ftl"))?, first);
        // An error finding fails the package instead of shipping broken text.
        std::fs::write(loc.join("fr").join("broken.ftl"), "hello = { $\n")?;
        assert!(copy_raw_assets(&project, &base.join("package3/assets")).is_err());
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    #[test]
    fn dialogues_ship_an_index_and_broken_ones_fail_the_package() -> Result<()> {
        let base = regia_test_utils::unique_temp_dir("package-dialogue-index");
        let project = base.join("project");
        let dialogue = project.join("assets/dialogue");
        std::fs::create_dir_all(&dialogue)?;
        std::fs::write(
            dialogue.join("intro.dialogue.ron"),
            r#"(format_version: 1, name: "intro", lines: [
                (id: "hello", speaker: "guard", variants: [(sound: "audio/hello.ogg")]),
            ])"#,
        )?;
        let dest = base.join("package/assets");
        copy_raw_assets(&project, &dest)?;
        let text = std::fs::read_to_string(dest.join("loc/dialogue_index.ron"))?;
        let index: regia::framework::dialogue::DialogueIndex = ron::from_str(&text)?;
        assert_eq!(index.files, ["dialogue/intro.dialogue.ron"]);
        assert!(
            dest.join("dialogue/intro.dialogue.ron").is_file(),
            "the file itself ships"
        );
        std::fs::write(dialogue.join("bad.voice.ron"), "(format_version: 9)")?;
        let err = copy_raw_assets(&project, &base.join("package2/assets")).expect_err("broken");
        assert!(err.to_string().contains("bad.voice.ron"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    #[test]
    fn raw_ability_sources_never_ship() -> Result<()> {
        // ADR-9850 (c): the authored ability formats stay out of every package, beside files that
        // ship raw as before.
        let base = regia_test_utils::unique_temp_dir("package-ability-sources");
        let project = base.join("project");
        for (rel, text) in [
            ("assets/abilities/bolt.ability.ron", "(format_version: 1)"),
            ("assets/stats/Hero.ATTRS.ron", "(format_version: 1)"),
            ("assets/effects/poison.effect.ron", "(format_version: 1)"),
            ("assets/about.txt", "hello"),
        ] {
            let path = project.join(rel);
            std::fs::create_dir_all(path.parent().context("parent")?)?;
            std::fs::write(path, text)?;
        }
        let dest = base.join("package/assets");
        copy_raw_assets(&project, &dest)?;
        assert!(dest.join("about.txt").is_file(), "other raw files still ship");
        for rel in [
            "abilities/bolt.ability.ron",
            "stats/Hero.ATTRS.ron",
            "effects/poison.effect.ron",
        ] {
            assert!(!dest.join(rel).exists(), "{rel} shipped raw");
        }
        // A named profile's closure that lists one is stripped the same way.
        let only: std::collections::BTreeSet<String> =
            ["abilities/bolt.ability.ron".to_owned(), "about.txt".to_owned()].into();
        let named = base.join("named/assets");
        stage_raw_files(&project, &named, &only, Profile::Shipping, "linux")?;
        assert!(named.join("about.txt").is_file());
        assert!(!named.join("abilities/bolt.ability.ron").exists());
        std::fs::remove_dir_all(&base)?;
        Ok(())
    }

    #[test]
    fn the_ability_strip_rule_and_message() {
        for rel in ["a/b.ability.ron", "B.Effect.RON", "hero.attrs.ron"] {
            assert!(is_raw_ability_source(Path::new(rel)), "{rel}");
        }
        for rel in ["a/b.ability.ron.bak", "ability.ron", "table.ron", "b.abilities"] {
            assert!(!is_raw_ability_source(Path::new(rel)), "{rel}");
        }
        let message = raw_ability_stripped("abilities/bolt.ability.ron");
        assert!(message.contains("`abilities/bolt.ability.ron`"), "{message}");
        assert!(message.contains("cooked ability assets only"), "{message}");
    }

    #[test]
    fn conversations_ship_cooked_and_a_required_missing_clip_fails() -> Result<()> {
        let base = regia_test_utils::unique_temp_dir("package-convc");
        let project = base.join("project");
        let conv = project.join("assets/conversations");
        std::fs::create_dir_all(&conv)?;
        std::fs::write(
            conv.join("intro.conv.ron"),
            r#"(format_version: 1, id: "intro", lines: [(id: "hello", speaker: "guard", text: "conv-hello")])"#,
        )?;
        let dest = base.join("package/assets");
        copy_raw_assets(&project, &dest)?;
        let bytes = std::fs::read(dest.join("conversations/intro.convc"))?;
        let cooked = regia::framework::conversation::cooked::CookedConversation::from_bytes(&bytes)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(cooked.asset.id, "intro");
        assert!(cooked.clips.contains_key("en"), "the source locale is staged");
        assert!(
            !dest.join("conversations/intro.conv.ron").exists(),
            "the source stays out"
        );
        std::fs::write(
            conv.join("conversation.ron"),
            "(format_version: 1, clip_policy: Require)",
        )?;
        let err = copy_raw_assets(&project, &base.join("package2/assets")).expect_err("missing clip");
        assert!(err.to_string().contains("conv_missing_clip"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    #[test]
    fn raw_assets_are_copied_but_cookable_and_meta_are_not() -> Result<()> {
        let base = std::env::temp_dir().join(format!("regia_cli_raw_assets_{}", std::process::id()));
        let project = base.join("project");
        let dest = base.join("package/assets");
        std::fs::create_dir_all(project.join("assets/meshes"))?;
        std::fs::create_dir_all(project.join("assets/config"))?;
        std::fs::write(project.join("assets/about.txt"), "about")?;
        std::fs::write(project.join("assets/config/touch.ron"), "x = 1")?;
        std::fs::write(project.join("assets/meshes/hero.glb"), b"not really a glb")?;
        std::fs::write(project.join("assets/meshes/hero.glb.meta"), "meta")?;

        let copied = copy_raw_assets(&project, &dest)?;
        assert_eq!(copied, 2, "only the two raw files should be copied");
        assert!(dest.join("about.txt").is_file());
        assert!(dest.join("config/touch.ron").is_file());
        assert!(!dest.join("meshes/hero.glb").exists());
        assert!(!dest.join("meshes/hero.glb.meta").exists());

        // A buffer a `.gltf` references is cooked with it and not shipped raw.
        std::fs::write(
            project.join("assets/meshes/helmet.gltf"),
            br#"{"asset":{"version":"2.0"},"buffers":[{"uri":"helmet.bin","byteLength":4}]}"#,
        )?;
        std::fs::write(project.join("assets/meshes/helmet.bin"), [0u8; 4])?;
        let dest2 = base.join("package2/assets");
        assert_eq!(copy_raw_assets(&project, &dest2)?, 2);
        assert!(!dest2.join("meshes/helmet.bin").exists());

        // Textures the cook takes do not ship raw too (ADR-1855); sounds do: the runtime reads
        // them from their files and the cook only measures them.
        std::fs::write(project.join("assets/meshes/a.dds"), b"DDS ")?;
        std::fs::write(project.join("assets/meshes/b.tga"), b"tga")?;
        std::fs::write(project.join("assets/meshes/c.bmp"), b"BM")?;
        std::fs::write(project.join("assets/door.wav"), b"RIFF")?;
        let dest3 = base.join("package3/assets");
        assert_eq!(copy_raw_assets(&project, &dest3)?, 3);
        assert!(dest3.join("door.wav").is_file());
        assert!(!dest3.join("meshes/a.dds").exists());
        assert!(!dest3.join("meshes/b.tga").exists());
        assert!(!dest3.join("meshes/c.bmp").exists());

        let empty = base.join("noassets");
        std::fs::create_dir_all(&empty)?;
        assert_eq!(copy_raw_assets(&empty, &base.join("pkg2/assets"))?, 0);

        // A level directory ships folded into one `.level.ron` (spec 37 § 3.6), never as entity files.
        let level_file = project.join("assets/forest.level.ron");
        std::fs::write(
            &level_file,
            regia_scene::format::SceneFile::new().to_ron_string()?,
        )?;
        regia_scene::level_dir::split_level_file(&level_file, &project.join("assets/forest.level.d"))?;
        std::fs::remove_file(&level_file).ok();
        let dest3 = base.join("package3/assets");
        copy_raw_assets(&project, &dest3)?;
        assert!(dest3.join("forest.level.ron").is_file());
        assert!(!dest3.join("forest.level.d").exists());
        assert!(
            regia_scene::format::SceneFile::from_ron_str(&std::fs::read_to_string(
                dest3.join("forest.level.ron")
            )?)
            .is_ok()
        );

        // Bakes do not ship as loose files: the cook puts them in the pak (ADR-7214; Android reads
        // no loose files from its APK). `tests/procgen_pak.rs` checks the pak and the load.
        let bakes = regia_procgen::world::level_bake_dir(Path::new("forest.level.ron"));
        std::fs::create_dir_all(project.join("assets").join(&bakes))?;
        std::fs::write(project.join("assets").join(&bakes).join("g.pgout"), b"RPGO")?;
        let dest4 = base.join("package4/assets");
        copy_raw_assets(&project, &dest4)?;
        assert!(!dest4.join(&bakes).join("g.pgout").exists());

        // An `editor_only` generator leaves the level and its bakes stay behind (ADR-2421).
        {
            use regia_procgen::world::ProcGenerator;
            use regia_scene::format::{SceneEntity, SceneFile};
            let raw = |editor_only: bool| {
                ron::value::RawValue::from_boxed_ron(
                    format!("(graph: \"g.pgraph.ron\", editor_only: {editor_only})").into(),
                )
            };
            let entity = |guid: u128, parent: Option<u128>, generator: Option<bool>| -> Result<SceneEntity> {
                let mut components = std::collections::BTreeMap::new();
                if let Some(e) = generator {
                    use regia_ecs::reflect::TypePath;
                    components.insert(ProcGenerator::type_path().to_string(), raw(e)?);
                }
                Ok(SceneEntity {
                    guid: regia_core::id::Guid(guid),
                    name: None,
                    parent: parent.map(regia_core::id::Guid),
                    components,
                    prefab_instance: None,
                })
            };
            let mut scene = SceneFile::new();
            scene.entities = vec![
                entity(0xa1, None, Some(false))?,
                entity(0xb2, None, Some(true))?,
                entity(0xc3, Some(0xb2), None)?,
            ];
            // Its own level: `forest` exists above in directory form.
            std::fs::write(project.join("assets/grove.level.ron"), scene.to_ron_string()?)?;
            let bakes = regia_procgen::world::level_bake_dir(Path::new("grove.level.ron"));
            let bake_dir = project.join("assets").join(&bakes);
            std::fs::create_dir_all(&bake_dir)?;
            let (kept, dropped) = (regia_core::id::Guid(0xa1), regia_core::id::Guid(0xb2));
            std::fs::write(bake_dir.join(format!("{kept}.pgout")), b"RPGO")?;
            std::fs::write(bake_dir.join(format!("{dropped}.pgout")), b"RPGO")?;
            std::fs::write(bake_dir.join(format!("{dropped}.High.pgout")), b"RPGO")?;
            let dest5 = base.join("package5/assets");
            copy_raw_assets(&project, &dest5)?;
            // No bake ships loose (ADR-7214): the cook leaves the editor-only ones out of the pak.
            assert!(!dest5.join(&bakes).join(format!("{kept}.pgout")).exists());
            assert!(!dest5.join(&bakes).join(format!("{dropped}.pgout")).exists());
            assert!(!dest5.join(&bakes).join(format!("{dropped}.High.pgout")).exists());
            assert!(regia_procgen::cook::bake_ships(
                &bake_dir.join(format!("{kept}.pgout"))
            ));
            assert!(!regia_procgen::cook::bake_ships(
                &bake_dir.join(format!("{dropped}.pgout"))
            ));
            assert!(!regia_procgen::cook::bake_ships(
                &bake_dir.join(format!("{dropped}.High.pgout"))
            ));
            let shipped = SceneFile::from_ron_str(&std::fs::read_to_string(dest5.join("grove.level.ron"))?)?;
            let guids: Vec<u128> = shipped.entities.iter().map(|e| e.guid.0).collect();
            assert_eq!(
                guids,
                vec![0xa1],
                "the editor-only generator and its child are gone"
            );
        }

        std::fs::remove_dir_all(&base)?;
        Ok(())
    }

    /// A font ships at its raw path as the cook's subset, and no pak holds it (ADR-2697).
    #[test]
    fn a_package_ships_the_subset_font_at_its_raw_path_and_not_in_a_pak() -> Result<()> {
        use regia_asset_pipeline::font_subset_fixture::tiny_font;
        let project = regia_test_utils::unique_temp_dir("package_font_subset");
        let write = |rel: &str, bytes: &[u8]| -> Result<()> {
            let path = project.join(rel);
            std::fs::create_dir_all(path.parent().context("parent")?)?;
            std::fs::write(path, bytes)?;
            Ok(())
        };
        let source = tiny_font();
        write("assets/fonts/ui.ttf", &source)?;
        write("assets/localization/en/main.ftl", b"greet = hi\n")?;
        write("assets/localization/fr/main.ftl", b"greet = fi\n")?;
        write("assets/about.txt", b"raw file")?;
        let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
            project_root: project.clone(),
            platform: "linux".to_string(),
            incremental: true,
        })?;
        super::super::cook::report_cook_outcome(&report)?;
        assert_eq!(report.font_subsets.len(), 1, "{:?}", report.font_subsets);
        let pak_dir = project.join("build/paks/linux");
        regia_asset_pipeline::pack_cooked_directory(
            &regia_asset::cooked_cache_dir(&project, "linux"),
            &pak_dir,
            &std::collections::BTreeMap::new(),
            true,
        )?;
        let reader = regia_asset::PakReader::open(&pak_dir.join("base.fpak"))?;
        assert_eq!(reader.toc.entries.len(), 0, "a font never enters a pak");

        let pkg = project.join("pkg");
        let assets = pkg.join("assets");
        let plain = build_profile::ResolvedProfile {
            name: "dev".into(),
            cargo: Profile::Dev,
            spec: None,
            server: false,
        };
        stage_content(
            &project,
            "linux",
            &plain,
            &pkg,
            &pkg.join("content"),
            &assets,
            || copy_raw_assets(&project, &assets),
        )?;
        let shipped = std::fs::read(assets.join("fonts/ui.ttf"))?;
        assert_eq!(shipped.len() as u64, report.font_subsets[0].bytes_after);
        assert!(shipped.len() < source.len());
        assert!(skrifa::raw::FontRef::new(&shipped).is_ok());
        assert_eq!(
            std::fs::read(project.join("assets/fonts/ui.ttf"))?,
            source,
            "the source stays"
        );
        assert!(assets.join("about.txt").is_file());
        assert!(!assets.join("fonts/ui.ttf.meta").exists());
        let _ = std::fs::remove_dir_all(project);
        Ok(())
    }

    /// A project with a level that binds a layer and places a functional test.
    fn staged_project(name: &str) -> Result<PathBuf> {
        let project = regia_test_utils::unique_temp_dir(name);
        let assets = project.join("assets");
        std::fs::create_dir_all(&assets)?;
        let defs_id = regia_core::id::Guid::from_u128(0xd0);
        let defs = assets.join("forest.layers.ron");
        std::fs::write(
            &defs,
            r#"LayerDefs(format_version: 1, layers: [
                (name: "night", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: None),
                (name: "blockout", kind: EditorOnly, debug_color: (0.0, 0.0, 0.0), parent: None),
            ])"#,
        )?;
        let sidecar = regia_asset::MetaSidecar::new("layers", regia_asset::AssetId(defs_id));
        std::fs::write(regia_asset::meta_path_for(&defs), sidecar.to_ron_string()?)?;
        let record = |id: u128, components: &str| {
            format!(
                r#"(guid: "{}", components: {{ {components} }}),"#,
                regia_core::id::Guid::from_u128(id)
            )
        };
        let level = format!(
            r#"SceneFile(format_version: {v}, entities: [{night} {editor} {test}], resources: {{
                "regia_scene::layers::LevelLayers": (defs: ["asset:{defs_id}"], entries: []),
            }})"#,
            v = regia_scene::format::SCENE_FORMAT_VERSION,
            night = record(
                1,
                r#""regia_scene::layers::LayerMembership": (layers: ["night"]),"#
            ),
            editor = record(
                2,
                r#""regia_scene::layers::LayerMembership": (layers: ["blockout"]),"#
            ),
            test = record(
                3,
                r#""regia_framework::functional::FunctionalTest": (format_version: 1),"#
            ),
        );
        std::fs::write(assets.join("forest.level.ron"), level)?;
        std::fs::write(
            assets.join("plain.level.ron"),
            format!(
                "SceneFile(format_version: {}, entities: [])\n",
                regia_scene::format::SCENE_FORMAT_VERSION
            ),
        )?;
        std::fs::write(
            assets.join("door.functest.ron"),
            "FuncTestScript(format_version: 1, steps: [])",
        )?;
        Ok(project)
    }

    fn staged_level(dest: &Path) -> Result<regia_scene::format::SceneFile> {
        Ok(regia_scene::format::SceneFile::from_ron_str(
            &std::fs::read_to_string(dest.join("forest.level.ron"))?,
        )?)
    }

    #[test]
    fn a_shipping_package_strips_tests_and_editor_only_members() -> Result<()> {
        let project = staged_project("package_strip")?;
        let dest = project.join("out");
        stage_raw_assets(&project, &dest, Profile::Shipping, "linux")?;
        let scene = staged_level(&dest)?;
        let guids: Vec<u128> = scene.entities.iter().map(|e| e.guid.as_u128()).collect();
        assert_eq!(guids, [1], "the test and the editor-only member are gone");
        let binding = regia_scene::layers::LevelLayers::from_scene(&scene)
            .expect("bound")
            .expect("parses");
        assert!(binding.defs.is_empty(), "the definitions travel in the level");
        assert_eq!(binding.embedded[0].layers.len(), 1);
        assert!(!dest.join("door.functest.ron").exists());
        assert!(!dest.join("forest.layers.ron").exists());
        std::fs::remove_dir_all(&project)?;
        Ok(())
    }

    #[test]
    fn a_dev_package_keeps_its_tests_and_drops_editor_only_members() -> Result<()> {
        let project = staged_project("package_dev")?;
        let dest = project.join("out");
        stage_raw_assets(&project, &dest, Profile::Dev, "linux")?;
        let scene = staged_level(&dest)?;
        let guids: Vec<u128> = scene.entities.iter().map(|e| e.guid.as_u128()).collect();
        assert_eq!(guids, [1, 3]);
        assert!(dest.join("door.functest.ron").exists());
        std::fs::remove_dir_all(&project)?;
        Ok(())
    }

    #[test]
    fn package_review_notes_are_removed_in_dev_and_shipping() -> Result<()> {
        let project = staged_project("package_review_notes")?;
        let source = project.join("assets/plain.level.ron");
        let mut scene = regia_scene::format::SceneFile::from_ron_str(&std::fs::read_to_string(&source)?)?;
        let note = regia_scene::format::SceneEntity {
            guid: regia_core::id::Guid::from_u128(0x123456),
            components: std::collections::BTreeMap::from([(
                "regia_editor::review_notes::ReviewNote".to_owned(),
                ron::value::RawValue::from_boxed_ron(
                    "(text: \"private review\", author: \"Author\", created: 1, resolved: false, thread: [])"
                        .into(),
                )?,
            )]),
            ..Default::default()
        };
        scene.entities.push(note);
        std::fs::write(&source, scene.to_ron_string()?)?;
        let prefab = project.join("assets/review.prefab.ron");
        std::fs::write(&prefab, scene.to_ron_string()?)?;
        for (name, profile) in [("dev", Profile::Dev), ("shipping", Profile::Shipping)] {
            let dest = project.join(name);
            stage_raw_assets(&project, &dest, profile, "linux")?;
            let output = std::fs::read_to_string(dest.join("plain.level.ron"))?;
            assert!(!output.contains("private review") && !output.contains("ReviewNote"));
            let output = std::fs::read_to_string(dest.join("review.prefab.ron"))?;
            assert!(!output.contains("private review") && !output.contains("ReviewNote"));
        }
        assert!(std::fs::read_to_string(source)?.contains("private review"));
        assert!(std::fs::read_to_string(prefab)?.contains("private review"));
        std::fs::remove_dir_all(project)?;
        Ok(())
    }

    #[test]
    fn a_plain_level_ships_byte_for_byte() -> Result<()> {
        let project = staged_project("package_plain")?;
        let dest = project.join("out");
        stage_raw_assets(&project, &dest, Profile::Shipping, "linux")?;
        assert_eq!(
            std::fs::read(dest.join("plain.level.ron"))?,
            std::fs::read(project.join("assets/plain.level.ron"))?
        );
        std::fs::remove_dir_all(&project)?;
        Ok(())
    }

    /// A level with a layer file (ADR-7792) ships folded: the `night` member that lives in
    /// `forest.night.layer.ron` is in the shipped level (and the editor-only member is still
    /// stripped), and no `.layer.ron` ships.
    #[test]
    fn a_level_with_a_layer_file_ships_folded_in() -> Result<()> {
        let project = staged_project("package_layer_file")?;
        let level = project.join("assets/forest.level.ron");
        let text = regia_scene::level_dir::read_level_as_stored(&level)?.to_ron_string()?;
        let separated = std::collections::BTreeSet::from(["night".to_owned()]);
        regia_scene::layer_file::write_split_level(
            &level,
            &text,
            &separated,
            &regia_scene::layer_file::SplitTarget::All,
            false,
        )?;
        let night = regia_core::id::Guid::from_u128(1).to_string();
        assert!(!std::fs::read_to_string(&level)?.contains(&night));
        assert!(project.join("assets/forest.night.layer.ron").is_file());
        let dest = project.join("out");
        stage_raw_assets(&project, &dest, Profile::Shipping, "linux")?;
        let scene = staged_level(&dest)?;
        let guids: Vec<u128> = scene.entities.iter().map(|e| e.guid.as_u128()).collect();
        assert_eq!(
            guids,
            [1],
            "the layer file's member ships; the editor-only one does not"
        );
        assert!(
            !dest.join("forest.night.layer.ron").exists(),
            "layer files are folded, not shipped"
        );
        std::fs::remove_dir_all(&project)?;
        Ok(())
    }

    #[test]
    fn both_forms_of_a_level_stop_the_package() -> Result<()> {
        let project = staged_project("package_ambiguous")?;
        let dir = project.join("assets/plain.level.d");
        regia_scene::level_dir::split_level_file(&project.join("assets/plain.level.ron"), &dir)?;
        let err =
            stage_raw_assets(&project, &project.join("out"), Profile::Dev, "linux").expect_err("ambiguous");
        assert!(format!("{err:#}").contains("plain.level"), "{err:#}");
        std::fs::remove_dir_all(&project)?;
        Ok(())
    }

    #[test]
    fn invalid_layer_definitions_stop_the_package() -> Result<()> {
        let project = staged_project("package_bad_defs")?;
        std::fs::write(
            project.join("assets/forest.layers.ron"),
            r#"LayerDefs(format_version: 1, layers: [(name: "a", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: Some("ghost"))])"#,
        )?;
        let err =
            stage_raw_assets(&project, &project.join("out"), Profile::Dev, "linux").expect_err("bad defs");
        assert!(format!("{err:#}").contains("layer_parent_invalid"), "{err:#}");
        std::fs::remove_dir_all(&project)?;
        Ok(())
    }

    #[test]
    fn desktop_entry_rejects_control_characters() {
        let ok = desktop_entry("Demo Game", "demo").expect("plain name");
        assert!(ok.contains("Name=Demo Game\n"));
        assert!(desktop_entry("Demo\nExec=sh -c id", "demo").is_err());
        assert!(desktop_entry("Demo\r", "demo").is_err());
        assert!(desktop_entry("Demo", "de\nmo").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn raw_asset_copy_skips_symlinks() -> Result<()> {
        let base = regia_test_utils::unique_temp_dir("regia_cli_raw_symlink");
        let project = base.join("project");
        let outside = base.join("outside");
        std::fs::create_dir_all(project.join("assets"))?;
        std::fs::create_dir_all(&outside)?;
        std::fs::write(outside.join("secret.txt"), "secret")?;
        std::fs::write(project.join("assets/ok.txt"), "ok")?;
        std::os::unix::fs::symlink(outside.join("secret.txt"), project.join("assets/link.txt"))?;
        std::os::unix::fs::symlink(&outside, project.join("assets/linkdir"))?;

        let dest = base.join("package/assets");
        assert_eq!(copy_raw_assets(&project, &dest)?, 1);
        assert!(dest.join("ok.txt").is_file());
        assert!(!dest.join("link.txt").exists());
        assert!(!dest.join("linkdir").exists());

        let apk = base.join("apk");
        assert_eq!(copy_dir(&project.join("assets"), &apk)?, 1);
        assert!(!apk.join("link.txt").exists());
        assert!(!apk.join("linkdir").exists());

        std::fs::remove_dir_all(&base)?;
        Ok(())
    }

    #[test]
    fn shipping_requires_a_signing_keystore() -> Result<()> {
        let no_key = Manifest::parse("[project]\nname = \"x\"\nid = \"a.b\"\n")?
            .platforms
            .android;
        // Dev builds use the AGP debug keystore and need no project key.
        assert!(require_signing_keystore(Profile::Dev, &no_key).is_ok());
        // Shipping without a keystore must error, not silently fall back (AGP 8 leaves release
        // unsigned — there is no debug fallback).
        let err = match require_signing_keystore(Profile::Shipping, &no_key) {
            Ok(()) => String::new(),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("keystore"), "{err}");
        assert!(err.contains("unsigned"), "{err}");

        // A literal keystore path (env vars resolve the same way) satisfies the requirement.
        let with_key = Manifest::parse(
            "[project]\nname = \"x\"\nid = \"a.b\"\n[platforms.android]\nkeystore = \"/keys/upload.jks\"\n",
        )?
        .platforms
        .android;
        assert!(require_signing_keystore(Profile::Shipping, &with_key).is_ok());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // pak freshness tests (real cook + pack, not a hand-built DB)
    // -----------------------------------------------------------------------

    /// A scratch project for cook/pack/freshness tests, removed on drop.
    struct PakProject(PathBuf);
    impl PakProject {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("regia_cli_pakfresh_{label}_{}_{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("assets")).expect("assets dir");
            PakProject(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn pak(&self) -> PathBuf {
            self.0.join("build/paks/linux/base.fpak")
        }
    }
    impl Drop for PakProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write a `.material.ron` source under `assets/` with the given material name.
    fn write_material(project: &Path, rel: &str, name: &str) {
        let mut mat = regia_asset::MaterialAssetRon::default_standard();
        mat.name = Some(name.to_string());
        let text = mat.to_ron_string().expect("material ron");
        let path = project.join("assets").join(rel);
        std::fs::create_dir_all(path.parent().expect("material parent")).expect("dir");
        std::fs::write(path, text).expect("write material");
    }

    /// Cook + pack the project for Linux into `build/paks/linux/base.fpak`.
    fn cook_and_pack(project: &Path) -> Result<()> {
        cook_and_pack_for(project, "linux")
    }

    /// Cook + pack the project for `platform` into `build/paks/<platform>/base.fpak`.
    fn cook_and_pack_for(project: &Path, platform: &str) -> Result<()> {
        let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
            project_root: project.to_path_buf(),
            platform: platform.to_string(),
            incremental: true,
        })?;
        assert!(report.errors.is_empty(), "cook errors: {:?}", report.errors);
        let cache = regia_asset::cooked_cache_dir(project, platform);
        let pak_dir = project.join("build/paks").join(platform);
        regia_asset_pipeline::pack_cooked_directory(
            &cache,
            &pak_dir,
            &std::collections::BTreeMap::new(),
            true,
        )?;
        Ok(())
    }

    /// The shader-permutation budget (ADR-2026) stops a shipping client package. A dedicated
    /// server of the same project draws nothing, so it packages.
    #[test]
    fn a_server_package_skips_the_permutation_budget() -> Result<()> {
        let dir = regia_test_utils::unique_temp_dir("package_server_permutations");
        let materials = dir.join("assets/materials");
        std::fs::create_dir_all(&materials)?;
        std::fs::write(
            materials.join("panel.matgraph.ron"),
            r#"(
    format_version: 2,
    name: "panel",
    nodes: { "c": Vec3((0.6, 0.55, 0.5)) },
    output: (base_color: Some("c")),
)"#,
        )?;
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"x\"\nid = \"a.b\"\n\n\
             [cook.permutation_budget.DesktopLow]\nmax_pipelines = 1\n",
        )?;
        let mut args = package_args(&dir);
        args.dry_run = true;
        args.profile_name = "shipping".to_string();
        args.profile = Profile::Shipping;
        let err = run(&args).expect_err("one graph is over a budget of one pipeline");
        assert!(format!("{err:#}").contains("over budget"), "{err:#}");
        args.server = true;
        run(&args)?;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// A `PackageArgs` for Linux packaging of `project`.
    fn package_args(project: &Path) -> PackageArgs {
        PackageArgs {
            platform: Platform::Linux,
            format: None,
            profile_name: "dev".to_string(),
            profile: Profile::Dev,
            dry_run: false,
            project: project.to_path_buf(),
            out: None,
            steam: false,
            abis: Vec::new(),
            glibc: None,
            pipeline_caches: None,
            pipeline_cache_references: Vec::new(),
            prepare_scenes: Vec::new(),
            server: false,
            json: false,
            audit_dir: None,
            audit_artifact: None,
            verify_receipt: None,
            container: None,
            container_image: None,
            container_mounts: Vec::new(),
            no_engine_credit: false,
        }
    }

    /// `regia package --json`'s object (ADR-7217): the artifact as an absolute path, the symbols
    /// directory beside it and the `build_info.json` inside a directory package when they exist.
    #[test]
    fn the_json_result_names_the_artifact_symbols_and_build_info() {
        let dir = regia_test_utils::unique_temp_dir("package-json-result");
        let staging = dir.join("Game");
        std::fs::create_dir_all(&staging).expect("staging");
        let args = package_args(&dir);
        let bare = json_result(&args, &staging);
        assert_eq!(bare["ok"], true);
        assert_eq!(bare["artifact"], staging.display().to_string());
        assert_eq!(bare["platform"], "linux");
        assert_eq!(bare["profile"], "dev");
        assert_eq!(
            (bare["server"].as_bool(), bare["dry_run"].as_bool()),
            (Some(false), Some(false))
        );
        assert!(
            bare["symbols"].is_null() && bare["build_info"].is_null(),
            "{bare}"
        );
        std::fs::create_dir_all(dir.join("symbols")).expect("symbols");
        std::fs::write(staging.join("build_info.json"), "{}").expect("build info");
        let full = json_result(&args, &staging);
        assert_eq!(full["symbols"], dir.join("symbols").display().to_string());
        assert_eq!(
            full["build_info"],
            staging.join("build_info.json").display().to_string()
        );
        assert!(!full.to_string().contains('\n'), "one line");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Write a minimal `.gltf` (one triangle) plus an external `.bin` buffer and a `.png` image it
    /// references by URI — the png is a glTF sidecar (cooked through the glTF, no DB row of its own).
    fn write_gltf_with_sidecars(project: &Path) {
        let dir = project.join("assets").join("meshes");
        std::fs::create_dir_all(&dir).expect("meshes dir");

        let mut bin = Vec::new();
        for p in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            for c in p {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        for i in [0u32, 1, 2] {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        std::fs::write(dir.join("scene.bin"), &bin).expect("bin");

        use image::ImageEncoder;
        let mut raw = Vec::with_capacity(16 * 4);
        for _ in 0..16 {
            raw.extend_from_slice(&[200, 30, 20, 255]);
        }
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&raw, 4, 4, image::ExtendedColorType::Rgba8)
            .expect("png");
        std::fs::write(dir.join("tex.png"), png).expect("png");

        std::fs::write(
            dir.join("scene.gltf"),
            r#"{
  "asset": {"version": "2.0"},
  "scene": 0,
  "scenes": [{"nodes": [0]}],
  "nodes": [{"mesh": 0}],
  "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1, "mode": 4}]}],
  "accessors": [
    {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0]},
    {"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}
  ],
  "bufferViews": [
    {"buffer": 0, "byteOffset": 0, "byteLength": 36},
    {"buffer": 0, "byteOffset": 36, "byteLength": 12}
  ],
  "buffers": [{"uri": "scene.bin", "byteLength": 48}],
  "images": [{"uri": "tex.png"}]
}"#,
        )
        .expect("gltf");
    }

    /// Set a file's mtime to an hour ago (so content is the only signal that matters).
    fn set_mtime_past(path: &Path) -> Result<()> {
        let past = SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(path)?
            .set_modified(past)?;
        Ok(())
    }

    #[test]
    fn fresh_base_pak_detection() -> Result<()> {
        let project = PakProject::new("fresh");
        let pak = project.pak();

        // Missing pak → not fresh.
        assert!(!base_pak_is_fresh(&pak, project.path()));

        // A real cook + pack of a project with no sources writes a valid zero-entry pak → not fresh.
        cook_and_pack(project.path())?;
        assert!(!base_pak_is_fresh(&pak, project.path()));

        // A real cook + pack of one source → fresh.
        write_material(project.path(), "materials/steel.material.ron", "Steel");
        cook_and_pack(project.path())?;
        assert!(base_pak_is_fresh(&pak, project.path()));

        // Source bytes change while the mtime is left in the past → stale (a content check, not mtime).
        let src = project.path().join("assets/materials/steel.material.ron");
        write_material(project.path(), "materials/steel.material.ron", "Steel2");
        set_mtime_past(&src)?;
        assert!(!base_pak_is_fresh(&pak, project.path()));

        // The cookable source is deleted while the pak still has entries → stale.
        std::fs::remove_file(&src)?;
        assert!(!base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    #[test]
    fn added_source_without_db_row_is_stale() -> Result<()> {
        let project = PakProject::new("added");
        let pak = project.pak();
        write_material(project.path(), "materials/steel.material.ron", "Steel");
        cook_and_pack(project.path())?;
        assert!(base_pak_is_fresh(&pak, project.path()));

        // A source added after the cook (no database row) is stale even with an mtime in the past.
        let added = project.path().join("assets/materials/copper.material.ron");
        write_material(project.path(), "materials/copper.material.ron", "Copper");
        set_mtime_past(&added)?;
        assert!(!base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    #[test]
    fn assets_path_as_file_is_stale() -> Result<()> {
        let project = PakProject::new("assetsfile");
        let pak = project.pak();
        write_material(project.path(), "materials/steel.material.ron", "Steel");
        cook_and_pack(project.path())?;
        assert!(base_pak_is_fresh(&pak, project.path()));

        // Remove the cooked source and save an *empty* database while the pak (still non-empty) and
        // the `assets/` directory remain. The pak must be no older than the database, so push the
        // database mtime into the past. With an empty database, the only signal that `assets/` is
        // now a file is the `!assets.is_dir()` guard: a removed guard would walk the file (which
        // yields no cookable sources), find an empty database with nothing missing, and wrongly
        // report fresh.
        std::fs::remove_file(project.path().join("assets/materials/steel.material.ron"))?;
        regia_asset_pipeline::AssetDatabase::default().save(project.path())?;
        set_mtime_past(&regia_asset::asset_db_path(project.path()))?;

        // Replace the assets directory with a plain file: the walk cannot enumerate sources → not fresh.
        std::fs::remove_dir_all(project.path().join("assets"))?;
        std::fs::write(project.path().join("assets"), "not a directory")?;
        assert!(!base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    #[test]
    fn missing_database_is_stale_even_without_sources() -> Result<()> {
        let project = PakProject::new("nodb");
        let pak = project.pak();
        write_material(project.path(), "materials/steel.material.ron", "Steel");
        cook_and_pack(project.path())?;
        assert!(base_pak_is_fresh(&pak, project.path()));

        // Delete both the source and the database: a non-empty pak with no sources and no database
        // rows must not be fresh (the old code opened the missing DB as empty and returned true).
        std::fs::remove_file(project.path().join("assets/materials/steel.material.ron"))?;
        std::fs::remove_file(regia_asset::asset_db_path(project.path()))?;
        assert!(!base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    #[test]
    fn pak_older_than_database_is_stale() -> Result<()> {
        let project = PakProject::new("dbmtime");
        let pak = project.pak();
        write_material(project.path(), "materials/steel.material.ron", "Steel");
        cook_and_pack(project.path())?;
        assert!(base_pak_is_fresh(&pak, project.path()));

        // The source hashes still match, but a pak older than the database (exactly what a failed
        // pack leaves behind: the database was saved, the pak was not rewritten) must not be fresh.
        set_mtime_past(&pak)?;
        assert!(!base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    #[test]
    fn deleted_source_is_pruned_so_next_pack_is_fresh() -> Result<()> {
        let project = PakProject::new("orphan");
        let pak = project.pak();
        write_material(project.path(), "materials/steel.material.ron", "Steel");
        write_material(project.path(), "materials/copper.material.ron", "Copper");

        let manifest = Manifest::parse("[project]\nname = \"x\"\nid = \"a.b\"\n")?;
        let args = package_args(project.path());

        // First package: cook + pack both sources → fresh.
        ensure_paks(project.path(), &manifest, &args)?;
        assert!(base_pak_is_fresh(&pak, project.path()));

        // Delete one source: the pak still holds both entries → not fresh.
        std::fs::remove_file(project.path().join("assets/materials/copper.material.ron"))?;
        assert!(!base_pak_is_fresh(&pak, project.path()));

        // Re-package: `cook_project` keeps the orphan row, but `ensure_paks` prunes it before packing,
        // so the next freshness check passes instead of recooking forever.
        ensure_paks(project.path(), &manifest, &args)?;
        assert!(base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    /// A project with a `Regia.toml` holding `chunks` (a `[cook.chunks]` table) and two
    /// materials: `materials/steel` (base) and `extra/copper` (the chunk's include).
    fn chunked_project(label: &str, chunks: &str) -> Result<PakProject> {
        let project = PakProject::new(label);
        write_material(project.path(), "materials/steel.material.ron", "Steel");
        write_material(project.path(), "extra/copper.material.ron", "Copper");
        std::fs::write(
            project.path().join("Regia.toml"),
            format!("[project]\nname = \"x\"\nid = \"a.b\"\n\n{chunks}"),
        )?;
        Ok(project)
    }

    const EXTRA_CHUNK: &str = "[cook.chunks.extra]\ninclude = [\"extra/**\"]\ndelivery = \"on_demand\"\n";

    /// deep-zai-assets finding 1 (ADR-7413): the chunk rules live in `Regia.toml`, so editing
    /// them must repack; the package used to skip the cook and ship the previous chunk set.
    #[test]
    fn editing_chunk_rules_in_regia_toml_repacks() -> Result<()> {
        let project = chunked_project("chunk_rules", EXTRA_CHUNK)?;
        let manifest = Manifest::parse("[project]\nname = \"x\"\nid = \"a.b\"\n")?;
        let args = package_args(project.path());
        let pak_dir = project.path().join("build/paks/linux");
        ensure_paks(project.path(), &manifest, &args)?;
        assert!(
            pak_dir.join("extra.fpak").is_file(),
            "the first pack writes the rule's chunk"
        );

        // Rename the chunk. Only `Regia.toml` changes (its mtime even goes back in time).
        let toml = project.path().join("Regia.toml");
        std::fs::write(
            &toml,
            std::fs::read_to_string(&toml)?.replace("chunks.extra", "chunks.bonus"),
        )?;
        set_mtime_past(&toml)?;
        ensure_paks(project.path(), &manifest, &args)?;
        assert!(
            pak_dir.join("bonus.fpak").is_file(),
            "the renamed chunk was never packed: the package shipped the old chunk set"
        );
        assert!(
            !pak_dir.join("extra.fpak").exists(),
            "the removed chunk is still staged"
        );
        Ok(())
    }

    /// ADR-7413: `regia cook` writes a complete pak directory, so it records the pack stamp and a
    /// following `regia package` skips its cook; an edit to `Regia.toml` after the cook does not.
    /// The cook is `--quality final`: only that one packs with the package's options (compressed,
    /// ADR-7612); a fast cook's paks are packed again (`a_fast_or_gpu_mesh_cook_is_not_fresh_for_package`).
    #[test]
    fn a_regia_cook_pack_is_fresh_until_regia_toml_changes() -> Result<()> {
        let project = chunked_project("cook_stamp", EXTRA_CHUNK)?;
        let pak_dir = project.path().join("build/paks/linux");
        let cook = crate::commands::cook::CookArgs {
            project: project.path().to_path_buf(),
            platform: Some("linux".into()),
            quality: "final".into(),
            clean: false,
            stats: false,
            json: false,
            ship: false,
            gpu_meshes: false,
            pack_report: false,
            size_report: false,
            lightmap_tiers: false,
            ids: None,
            texture_tier: Vec::new(),
            shared_cache: None,
            read_only: false,
            no_shared_cache: true,
            action: None,
        };
        crate::commands::cook::run(&cook)?;
        assert!(pak_dir.join("extra.fpak").is_file());
        assert!(
            pack_stamp_matches(project.path(), &pak_dir),
            "regia cook left no pack stamp"
        );

        let toml = project.path().join("Regia.toml");
        std::fs::write(&toml, format!("{}\n# edited\n", std::fs::read_to_string(&toml)?))?;
        assert!(
            !pack_stamp_matches(project.path(), &pak_dir),
            "a Regia.toml edit kept the pack fresh"
        );
        Ok(())
    }

    /// `regia cook` arguments for `project` on Linux with the quality and mesh choice given.
    fn cook_args(project: &Path, quality: &str, gpu_meshes: bool) -> crate::commands::cook::CookArgs {
        crate::commands::cook::CookArgs {
            project: project.to_path_buf(),
            platform: Some("linux".into()),
            quality: quality.into(),
            clean: false,
            stats: false,
            json: false,
            ship: false,
            gpu_meshes,
            pack_report: false,
            size_report: false,
            lightmap_tiers: false,
            ids: None,
            texture_tier: Vec::new(),
            shared_cache: None,
            read_only: false,
            no_shared_cache: true,
            action: None,
        }
    }

    /// rev-zai-rc-b package-present 1 (w9-pack-stamp-and-content): `regia cook --quality fast`
    /// packs uncompressed and `--gpu-meshes` stores GPU-ready meshes, while `regia package` always
    /// compresses and keeps the cooked meshes on Linux unless the environment asks. Uncompressed paks
    /// are not shipping paks: the stamp must not certify them, and the package packs again.
    /// GPU-ready meshes a cook chose are kept (`PackOptions::serves`).
    #[test]
    fn a_fast_or_gpu_mesh_cook_is_not_fresh_for_package() -> Result<()> {
        let project = chunked_project("cook_options", EXTRA_CHUNK)?;
        let pak_dir = project.path().join("build/paks/linux");
        let manifest = Manifest::parse("[project]\nname = \"x\"\nid = \"a.b\"\n")?;

        crate::commands::cook::run(&cook_args(project.path(), "fast", false))?;
        assert!(pak_dir.join("extra.fpak").is_file());
        assert!(
            !pack_stamp_matches(project.path(), &pak_dir),
            "the uncompressed paks of a fast cook were certified fresh for the package"
        );
        // The package packs again, with its own options, and that pack is fresh.
        ensure_paks(project.path(), &manifest, &package_args(project.path()))?;
        assert!(
            pack_stamp_matches(project.path(), &pak_dir),
            "the package's own pack is not fresh"
        );
        let stamp = std::fs::read_to_string(pak_dir.join(PACK_STAMP_FILE))?;
        assert!(stamp.contains("\ncompress 1\n"), "{stamp}");

        // A final cook with the package's choices counts for the package.
        crate::commands::cook::run(&cook_args(project.path(), "final", false))?;
        assert!(
            pack_stamp_matches(project.path(), &pak_dir),
            "a final cook no longer counts for the package"
        );
        // So does a final cook with GPU-ready meshes (what `regia cook --ship` packs): the
        // cooked-pack guide ships those paks; the stamp records the choice.
        crate::commands::cook::run(&cook_args(project.path(), "final", true))?;
        assert!(
            pack_stamp_matches(project.path(), &pak_dir),
            "the GPU-ready paks of a shipping cook were packed again"
        );
        let stamp = std::fs::read_to_string(pak_dir.join(PACK_STAMP_FILE))?;
        assert!(stamp.contains("\ngpu_meshes 1\n"), "{stamp}");
        Ok(())
    }

    /// ADR-7612: which recorded pack options serve which package. Compression must match; GPU-ready
    /// meshes serve a package that keeps cooked meshes, not the other way round (a package told to
    /// store GPU-ready meshes by `REGIA_PAK_GPU_MESHES` packs again over cooked ones).
    #[test]
    fn recorded_pack_options_serve_the_package_only_when_compressed() {
        let o = |compress, gpu_meshes| PackOptions { compress, gpu_meshes };
        // The package's usual options: compressed, cooked meshes.
        assert!(o(true, false).serves(o(true, false)));
        assert!(o(true, true).serves(o(true, false)), "a --ship cook's paks");
        assert!(!o(false, false).serves(o(true, false)), "a fast cook's paks");
        assert!(!o(false, true).serves(o(true, false)));
        // A package that wants GPU-ready meshes.
        assert!(o(true, true).serves(o(true, true)));
        assert!(!o(true, false).serves(o(true, true)), "cooked meshes were kept");
        assert!(!o(false, true).serves(o(true, true)));
    }

    /// rev-zai-rc-b package-present 3 (w9-pack-stamp-and-content): a project that still has only
    /// the legacy manifest loads its version (the content build id) from it, so the stamp must hash
    /// that file: a version bump there used to keep the old paks and ship the stale build id.
    #[test]
    fn a_legacy_manifest_edit_makes_the_pack_stale() -> Result<()> {
        let legacy = regia_core::brand::LEGACY_MANIFEST_FILE;
        if legacy == crate::manifest::MANIFEST_FILE {
            return Ok(());
        }
        let project = PakProject::new("legacy_manifest");
        write_material(project.path(), "materials/steel.material.ron", "Steel");
        let toml = project.path().join(legacy);
        std::fs::write(
            &toml,
            "[project]\nname = \"x\"\nid = \"a.b\"\nversion = \"1.0.0\"\n",
        )?;
        let manifest = Manifest::load(project.path())?;
        let pak_dir = project.path().join("build/paks/linux");
        ensure_paks(project.path(), &manifest, &package_args(project.path()))?;
        assert!(pack_stamp_matches(project.path(), &pak_dir));

        std::fs::write(
            &toml,
            "[project]\nname = \"x\"\nid = \"a.b\"\nversion = \"1.0.1\"\n",
        )?;
        set_mtime_past(&toml)?;
        assert!(
            !pack_stamp_matches(project.path(), &pak_dir),
            "a version bump in the legacy manifest kept the paks (and their build id) fresh"
        );
        Ok(())
    }

    /// deep-zai-assets finding 2 (ADR-7413): a pack that stopped after `base.fpak` was written
    /// (disk full, a cooked file gone, a crash while writing a later chunk) leaves a `base.fpak`
    /// newer than the database; the next package must pack again, not ship the leftovers.
    #[test]
    fn a_pack_stopped_after_base_fpak_is_not_fresh() -> Result<()> {
        let project = chunked_project("pack_stopped", EXTRA_CHUNK)?;
        let manifest = Manifest::parse("[project]\nname = \"x\"\nid = \"a.b\"\n")?;
        let args = package_args(project.path());
        let pak_dir = project.path().join("build/paks/linux");
        ensure_paks(project.path(), &manifest, &args)?;
        let extra_hash = || -> Result<String> {
            Ok(blake3::hash(&std::fs::read(pak_dir.join("extra.fpak"))?)
                .to_hex()
                .to_string())
        };
        let extra_before = extra_hash()?;

        // Change the chunk's asset, then run what `ensure_paks` runs up to the first chunk: cook,
        // save the pruned database, write `base.fpak`; the pack stops before `extra.fpak` and the
        // content manifest.
        write_material(project.path(), "extra/copper.material.ron", "Bronze");
        let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
            project_root: project.path().to_path_buf(),
            platform: "linux".to_string(),
            incremental: true,
        })?;
        assert!(report.errors.is_empty(), "cook errors: {:?}", report.errors);
        prune_asset_db(project.path())?;
        regia_asset_pipeline::pack_cooked_directory(
            &regia_asset::cooked_cache_dir(project.path(), "linux"),
            &pak_dir,
            &std::collections::BTreeMap::new(),
            true,
        )?;
        assert_eq!(extra_hash()?, extra_before);

        ensure_paks(project.path(), &manifest, &args)?;
        assert_ne!(
            extra_hash()?,
            extra_before,
            "the stopped pack was taken as fresh: extra.fpak still holds the old copper"
        );
        Ok(())
    }

    #[test]
    fn gltf_sidecar_has_no_db_row_and_is_fresh() -> Result<()> {
        let project = PakProject::new("sidecar");
        let pak = project.pak();
        write_gltf_with_sidecars(project.path());
        cook_and_pack(project.path())?;

        // The gltf is cooked (has a DB row); the png is a sidecar with no row → fresh.
        assert!(base_pak_is_fresh(&pak, project.path()));

        // Changing the gltf bytes (mtime left in the past) makes it stale.
        let gltf = project.path().join("assets/meshes/scene.gltf");
        let json = std::fs::read_to_string(&gltf)?;
        std::fs::write(&gltf, format!("{json} "))?;
        set_mtime_past(&gltf)?;
        assert!(!base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    #[test]
    fn scatter_ron_is_hashed_like_cook_one_records_it() -> Result<()> {
        let project = PakProject::new("scatter");
        let pak = project.pak();

        // A real cook + pack of a default scatter rule must be fresh: the pak-freshness check must
        // hash `.scatter.ron` the way `cook_one` records it (its `source_hash` is blake3 of the
        // bytes), not skip it (which would leave a recorded-but-absent row and recook forever).
        let rule = regia_terrain::ScatterRule::default().to_ron()?;
        let path = project.path().join("assets/scatter/rocks.scatter.ron");
        std::fs::create_dir_all(path.parent().expect("scatter dir"))?;
        std::fs::write(&path, &rule)?;
        cook_and_pack(project.path())?;
        assert!(base_pak_is_fresh(&pak, project.path()));

        // A byte change with the mtime left in the past → stale (a content check, not mtime).
        std::fs::write(&path, format!("{rule}\n// changed"))?;
        set_mtime_past(&path)?;
        assert!(!base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    /// Write a 64×64 16-bit grayscale heightfield PNG (deterministic bytes from `seed`).
    fn write_heightfield_png(path: &Path, seed: u32) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        const HF: u32 = 64;
        let mut raw = Vec::with_capacity((HF * HF * 2) as usize);
        let mut state = seed | 1;
        for _ in 0..HF * HF {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            raw.extend_from_slice(&((state >> 16) as u16).to_le_bytes());
        }
        use image::ImageEncoder;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&raw, HF, HF, image::ExtendedColorType::L16)
            .expect("heightfield png");
        std::fs::write(path, png).expect("write heightfield");
    }

    /// Write a minimal cookable terrain: a v1 layer file with one `ground` layer, a 16-bit
    /// heightfield, and its world-extents json. No macro or layer textures — the heightfield alone
    /// makes it a cookable terrain.
    fn write_minimal_terrain(project: &Path) {
        let tex = project.join("assets").join("textures");
        std::fs::create_dir_all(&tex).expect("textures dir");
        write_terrain_layer_file(&tex.join("terrain.layers.ron"));
        write_heightfield_png(&tex.join("heightfield.png"), 7);
        std::fs::write(tex.join("heightfield.json"), r#"{"size_m": 1000.0}"#).expect("heightfield.json");
    }

    /// Write a single-layer `terrain.layers.ron` at `path` (one `ground` layer, no textures).
    fn write_terrain_layer_file(path: &Path) {
        std::fs::write(
            path,
            r#"TerrainLayers(
    version: 1,
    wetness_default: 0.0,
    layers: [
        LayerDesc(
            name: "ground",
            albedo: (0.18, 0.28, 0.10),
            roughness: 0.95,
            tiling: 8.0,
            slope_min: 0.0,
            slope_max: 0.6,
            altitude_min: -1000.0,
            altitude_max: 1000.0,
            blend_sharpness: 0.15,
            triplanar: false,
        ),
    ],
)"#,
        )
        .expect("terrain.layers.ron");
    }

    #[test]
    fn minimal_terrain_cook_and_pack_is_fresh() -> Result<()> {
        let project = PakProject::new("terrain_fresh");
        let pak = project.pak();
        write_minimal_terrain(project.path());
        cook_and_pack(project.path())?;
        assert!(
            base_pak_is_fresh(&pak, project.path()),
            "a minimal terrain (layer file + heightfield + extents) must cook and pack fresh"
        );
        Ok(())
    }

    #[test]
    fn android_heightfield_cooks_tiled_and_stays_fresh() -> Result<()> {
        let project = PakProject::new("android_hf");
        let pak = project.path().join("build/paks/android/base.fpak");

        // A terrain layer file + heightfield. Android cooks both through the tiled path (the
        // heightfield stays R16; there is no texture slot to ASTC-encode). The heightfield is a
        // terrain input and has no generic database row. Freshness reads the platform from the
        // pak directory name.
        let tex = project.path().join("assets/textures");
        std::fs::create_dir_all(&tex).expect("textures dir");
        write_terrain_layer_file(&tex.join("terrain.layers.ron"));
        write_heightfield_png(&tex.join("heightfield.png"), 7);
        cook_and_pack_for(project.path(), "android")?;

        let db = regia_asset_pipeline::AssetDatabase::open(project.path())?;
        assert!(
            db.entries.contains_key("assets/textures/terrain.layers.ron"),
            "the android cook must record the terrain layer: {:?}",
            db.entries.keys()
        );
        assert!(
            !db.entries.contains_key("assets/textures/heightfield.png"),
            "the heightfield is a terrain input, not a generic row: {:?}",
            db.entries.keys()
        );
        assert!(base_pak_is_fresh(&pak, project.path()));

        // Changing the heightfield bytes (mtime left in the past) must invalidate the pak.
        let hf = project.path().join("assets/textures/heightfield.png");
        write_heightfield_png(&hf, 99);
        set_mtime_past(&hf)?;
        assert!(!base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    #[test]
    fn android_and_linux_share_the_terrain_layer_row() -> Result<()> {
        let project = PakProject::new("android_orphan");
        let linux_pak = project.pak();
        let android_pak = project.path().join("build/paks/android/base.fpak");

        // Both platforms cook the layer through the tiled path and leave the heightfield (a
        // terrain input) no row of its own. The database is shared, so after both cooks it holds
        // the layer row once. Tile payloads differ (BCn vs ASTC) and live in each platform's cache.
        let tex = project.path().join("assets/textures");
        std::fs::create_dir_all(&tex).expect("textures dir");
        write_terrain_layer_file(&tex.join("terrain.layers.ron"));
        write_heightfield_png(&tex.join("heightfield.png"), 7);

        cook_and_pack_for(project.path(), "linux")?;
        assert!(base_pak_is_fresh(&linux_pak, project.path()));

        cook_and_pack_for(project.path(), "android")?;
        let db = regia_asset_pipeline::AssetDatabase::open(project.path())?;
        assert!(
            db.entries.contains_key("assets/textures/terrain.layers.ron"),
            "the terrain-layer row must persist in the shared database: {:?}",
            db.entries.keys()
        );
        assert!(
            !db.entries.contains_key("assets/textures/heightfield.png"),
            "neither platform records heightfield.png generically: {:?}",
            db.entries.keys()
        );

        assert!(
            base_pak_is_fresh(&android_pak, project.path()),
            "the shared terrain-layer row must leave the android pak fresh"
        );
        // Android saved the shared database after the Linux pak was written. The existing
        // failed-pack guard must still reject that older pak. Make the ordering explicit even
        // on filesystems with coarse timestamp resolution; no sleep is needed.
        set_mtime_past(&linux_pak)?;
        assert!(!base_pak_is_fresh(&linux_pak, project.path()));
        let db_path = regia_asset::asset_db_path(project.path());
        let database_before_repack = std::fs::read(&db_path)?;
        regia_asset_pipeline::pack_cooked_directory(
            &regia_asset::cooked_cache_dir(project.path(), "linux"),
            linux_pak.parent().expect("linux pak directory"),
            &std::collections::BTreeMap::new(),
            true,
        )?;
        assert_eq!(std::fs::read(&db_path)?, database_before_repack);
        assert!(base_pak_is_fresh(&linux_pak, project.path()));
        assert!(base_pak_is_fresh(&android_pak, project.path()));

        // A second android ensure_paks must stay fresh.
        let manifest = Manifest::parse("[project]\nname = \"x\"\nid = \"a.b\"\n")?;
        let args = PackageArgs {
            platform: Platform::Android,
            format: None,
            profile_name: "dev".to_string(),
            profile: Profile::Dev,
            dry_run: false,
            project: project.path().to_path_buf(),
            out: None,
            steam: false,
            abis: Vec::new(),
            glibc: None,
            pipeline_caches: None,
            pipeline_cache_references: Vec::new(),
            prepare_scenes: Vec::new(),
            server: false,
            json: false,
            audit_dir: None,
            audit_artifact: None,
            verify_receipt: None,
            container: None,
            container_image: None,
            container_mounts: Vec::new(),
            no_engine_credit: false,
        };
        ensure_paks(project.path(), &manifest, &args)?;
        assert!(base_pak_is_fresh(&android_pak, project.path()));
        Ok(())
    }

    /// ADR-4914, kept by ADR-5294: Android cooks a terrain through the tiled path, so its pak goes
    /// stale when the world extents or the layer file change (neither has a generic row there).
    #[test]
    fn android_terrain_extents_and_layer_changes_are_stale() -> Result<()> {
        let project = PakProject::new("android_terrain");
        let pak = project.path().join("build/paks/android/base.fpak");
        write_minimal_terrain(project.path());
        cook_and_pack_for(project.path(), "android")?;
        assert!(base_pak_is_fresh(&pak, project.path()));

        let json = project.path().join("assets/textures/heightfield.json");
        std::fs::write(&json, r#"{"size_m": 2000.0}"#)?;
        set_mtime_past(&json)?;
        assert!(!base_pak_is_fresh(&pak, project.path()), "extents change");

        cook_and_pack_for(project.path(), "android")?;
        assert!(base_pak_is_fresh(&pak, project.path()));
        let layer = project.path().join("assets/textures/terrain.layers.ron");
        let text = std::fs::read_to_string(&layer)?;
        std::fs::write(&layer, format!("{text}\n// changed"))?;
        set_mtime_past(&layer)?;
        assert!(!base_pak_is_fresh(&pak, project.path()), "layer change");
        Ok(())
    }

    #[test]
    fn terrain_layer_change_is_stale() -> Result<()> {
        let project = PakProject::new("terrain_layer");
        let pak = project.pak();
        write_minimal_terrain(project.path());
        cook_and_pack(project.path())?;
        assert!(base_pak_is_fresh(&pak, project.path()));

        // Layer file bytes change (mtime left in the past) → stale via the recorded source hash.
        let layer = project.path().join("assets/textures/terrain.layers.ron");
        let text = std::fs::read_to_string(&layer)?;
        std::fs::write(&layer, format!("{text}\n// changed"))?;
        set_mtime_past(&layer)?;
        assert!(!base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    #[test]
    fn terrain_heightfield_change_is_stale() -> Result<()> {
        let project = PakProject::new("terrain_hf");
        let pak = project.pak();
        write_minimal_terrain(project.path());
        cook_and_pack(project.path())?;
        assert!(base_pak_is_fresh(&pak, project.path()));

        // Heightfield bytes change (mtime left in the past) → stale via the settings hash.
        let hf = project.path().join("assets/textures/heightfield.png");
        write_heightfield_png(&hf, 42);
        set_mtime_past(&hf)?;
        assert!(!base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    #[test]
    fn terrain_heightfield_json_change_is_stale() -> Result<()> {
        let project = PakProject::new("terrain_json");
        let pak = project.pak();
        write_minimal_terrain(project.path());
        cook_and_pack(project.path())?;
        assert!(base_pak_is_fresh(&pak, project.path()));

        // The world-extents json is a terrain input with no row of its own; a change (mtime in the
        // past) must still invalidate the terrain via its settings hash.
        let json = project.path().join("assets/textures/heightfield.json");
        std::fs::write(&json, r#"{"size_m": 2000.0}"#)?;
        set_mtime_past(&json)?;
        assert!(!base_pak_is_fresh(&pak, project.path()));
        Ok(())
    }

    #[test]
    fn fpc_from_bundle_merged_for_its_driver() -> Result<()> {
        use regia_framework::feedback::zip::ZipWriter;
        use regia_render::pipeline_cache::{PipelineCacheKey, WGPU_VERSION, encode, engine_shader_hash};
        let dir = regia_test_utils::unique_temp_dir("fpc-feedback-harvest");
        std::fs::create_dir_all(&dir)?;
        let key = |driver: &str| PipelineCacheKey {
            backend: "Vulkan".into(),
            vendor: 0x10de,
            device: 0x2c02,
            driver: "test-driver".into(),
            driver_info: driver.into(),
            wgpu_version: WGPU_VERSION.into(),
            shader_hash: engine_shader_hash(),
        };
        let mut bundle = ZipWriter::new(0);
        bundle.add("pipelines/older.fpc", &encode(&key("1"), &[1; 300]));
        bundle.add("pipelines/newer.fpc", &encode(&key("2"), &[2; 100]));
        bundle.add("pipelines/larger.fpc", &encode(&key("2"), &[3; 200]));
        bundle.add("note.txt", b"driver cache feedback");
        std::fs::write(dir.join("feedback.zip"), bundle.finish())?;
        let merged = collect_pipeline_caches(&dir)?;
        assert_eq!(merged.len(), 2, "feedback caches must retain both driver keys");
        assert_eq!(
            merged[&key("1").shipped_file_name()].2,
            encode(&key("1"), &[1; 300]).len() as u64
        );
        assert_eq!(
            merged[&key("2").shipped_file_name()].2,
            encode(&key("2"), &[3; 200]).len() as u64
        );
        let mut args = parse(&["--platform", "linux"])?;
        args.pipeline_caches = Some(dir.join("feedback.zip"));
        let content = dir.join("content");
        let report = stage_pipeline_caches(&dir, &args, &content)?;
        assert_eq!(report.len(), 2);
        for (driver, payload) in [("1", vec![1; 300]), ("2", vec![3; 200])] {
            let (loaded, state, _) = regia_render::pipeline_cache::initial_payload(
                &dir.join("no-local-cache"),
                Some(&content.join("pipelines")),
                &key(driver),
            );
            assert_eq!(loaded, Some(payload));
            assert!(matches!(
                state,
                regia_render::pipeline_cache::CacheLoad::Seeded(_)
            ));
        }
        let (wrong, _, _) = regia_render::pipeline_cache::initial_payload(
            &dir.join("no-local-cache"),
            Some(&content.join("pipelines")),
            &key("3"),
        );
        assert!(wrong.is_none(), "another driver must compile its own pipelines");
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn pipeline_caches_from_real_feedback_respect_build_references() -> Result<()> {
        use regia_framework::feedback::{
            BundleInput, FeedbackCategory, PipelineCacheAttachment, write_bundle,
        };
        use regia_render::pipeline_cache::{PipelineCacheKey, WGPU_VERSION, encode};
        let root = regia_test_utils::unique_temp_dir("fpc-build-reference");
        let key = PipelineCacheKey {
            backend: "Vulkan".into(),
            vendor: 0x1002,
            device: 0x1234,
            driver: "test-driver".into(),
            driver_info: "test-version".into(),
            wgpu_version: WGPU_VERSION.into(),
            shader_hash: "game-content-salt".into(),
        };
        let source = root.join("saved.fpc");
        let bytes = encode(&key, &[42; 500]);
        regia_test_utils::write_atomic(&source, &bytes)?;
        let mut input = BundleInput::new(FeedbackCategory::Performance, "pipeline capture");
        input.pipeline_cache = Some(PipelineCacheAttachment {
            path: source,
            key: key.clone(),
        });
        let bundle = write_bundle(&root.join("feedback"), &input, 0)?;
        let mut args = parse(&["--platform", "linux"])?;
        args.pipeline_caches = Some(bundle.path);
        let content = root.join("content");
        assert!(
            stage_pipeline_caches(&root, &args, &content)?.is_empty(),
            "salt requires a build reference"
        );

        // A reference from another GPU/driver authorizes the build's shader/wgpu pair only.
        let reference = root.join("build-reference.fpc");
        let farm_key = PipelineCacheKey {
            vendor: 0x10de,
            device: 1,
            driver_info: "farm".into(),
            ..key.clone()
        };
        regia_test_utils::write_atomic(&reference, &encode(&farm_key, &[7; 100]))?;
        args.pipeline_cache_references.push(reference.clone());
        let report = stage_pipeline_caches(&root, &args, &content)?;
        assert_eq!(report.len(), 1);
        assert_eq!(
            std::fs::read(content.join("pipelines").join(key.shipped_file_name()))?,
            bytes
        );
        assert_eq!(report[0]["driver_info"], "test-version");

        for wrong in [
            PipelineCacheKey {
                wgpu_version: "wrong-version".into(),
                ..farm_key.clone()
            },
            PipelineCacheKey {
                shader_hash: "wrong-shaders".into(),
                ..farm_key
            },
        ] {
            regia_test_utils::write_atomic(&reference, &encode(&wrong, &[7; 100]))?;
            assert!(stage_pipeline_caches(&root, &args, &root.join("rejected"))?.is_empty());
            assert!(!root.join("rejected/pipelines").exists());
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn pipeline_caches_are_validated_and_named_by_key() -> Result<()> {
        use regia_render::pipeline_cache::{PipelineCacheKey, WGPU_VERSION, encode, engine_shader_hash};
        let dir = regia_test_utils::unique_temp_dir("regia-pkg-fpc");
        std::fs::create_dir_all(&dir)?;
        let key = |driver: &str| PipelineCacheKey {
            backend: "Vulkan".into(),
            vendor: 0x10de,
            device: 0x2c02,
            driver: "NVIDIA".into(),
            driver_info: driver.into(),
            wgpu_version: WGPU_VERSION.into(),
            shader_hash: engine_shader_hash(),
        };
        // Two drivers of one GPU (same local file stem) stay apart; a duplicate keeps the larger.
        std::fs::write(dir.join("a.fpc"), encode(&key("580.95"), &[1u8; 300]))?;
        std::fs::write(dir.join("b.fpc"), encode(&key("590.10"), &[2u8; 100]))?;
        std::fs::write(dir.join("c.fpc"), encode(&key("590.10"), &[3u8; 200]))?;
        std::fs::write(dir.join("notes.txt"), "ignored")?;
        let staged = collect_pipeline_caches(&dir)?;
        assert_eq!(staged.len(), 2);
        let newer = &staged[&key("590.10").shipped_file_name()];
        assert!(newer.0.path.ends_with("c.fpc"));
        assert!(staged.contains_key(&key("580.95").shipped_file_name()));
        // A cache from this engine matches; one from another shader set or wgpu is reported.
        assert!(cache_matches_engine(&key("590.10")));
        let stale = PipelineCacheKey {
            shader_hash: "0123456789abcdef".into(),
            ..key("590.10")
        };
        assert!(!cache_matches_engine(&stale));
        let other_wgpu = PipelineCacheKey {
            wgpu_version: "29".into(),
            ..key("590.10")
        };
        assert!(!cache_matches_engine(&other_wgpu));
        // A corrupt cache fails the package instead of shipping.
        let mut torn = encode(&key("580.95"), &[1u8; 300]);
        let last = torn.len() - 1;
        torn[last] ^= 0xff;
        std::fs::write(dir.join("d.fpc"), torn)?;
        assert!(collect_pipeline_caches(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// The compiled-in copy is the repository's `THIRD_PARTY_NOTICES.md` (`cargo xtask notices` writes
    /// both). Only runs in a checkout: a packaged copy has no repository root.
    #[test]
    fn compiled_in_notices_match_the_repository_file() -> Result<()> {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        if !manifest.join("../../Cargo.toml").is_file() {
            return Ok(());
        }
        let root_copy = std::fs::read_to_string(manifest.join("../../THIRD_PARTY_NOTICES.md"))?;
        assert_eq!(ENGINE_NOTICES, root_copy, "run `cargo xtask notices`");
        Ok(())
    }

    #[test]
    fn a_game_workspace_without_notices_ships_the_engine_notices() -> Result<()> {
        let base = regia_test_utils::unique_temp_dir("cli-notices");
        let game = base.join("game");
        let engine = base.join("engine");
        let out_game = base.join("out-game");
        let out_engine = base.join("out-engine");
        for dir in [&game, &engine, &out_game, &out_engine] {
            std::fs::create_dir_all(dir)?;
        }
        // A `regia new` project: its own licence, no notices.
        std::fs::write(game.join("LICENSE.md"), "game licence\n")?;
        assert!(copy_notices_from(&game, &out_game)?, "the fallback is reported");
        let written = std::fs::read_to_string(out_game.join("THIRD_PARTY_NOTICES.md"))?;
        assert_eq!(written, ENGINE_NOTICES);
        assert!(
            written.starts_with("# Third-party notices"),
            "{}",
            &written[..40.min(written.len())]
        );
        assert_eq!(
            std::fs::read_to_string(out_game.join("LICENSE.md"))?,
            "game licence\n"
        );
        // A workspace with its own notices (the engine repository) ships those, unchanged.
        std::fs::write(
            engine.join("THIRD_PARTY_NOTICES.md"),
            "# Third-party notices\nlocal\n",
        )?;
        assert!(!copy_notices_from(&engine, &out_engine)?);
        assert_eq!(
            std::fs::read_to_string(out_engine.join("THIRD_PARTY_NOTICES.md"))?,
            "# Third-party notices\nlocal\n"
        );
        assert!(
            !out_engine.join("LICENSE.md").exists(),
            "no licence to copy, none invented"
        );
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    /// Spec 40 § 2.6: pack chunks of a chunked Android cook land in their asset-pack modules, install
    /// chunks do not; a cook without a manifest has no packs.
    #[test]
    fn android_pack_chunks_stage_into_their_modules() -> Result<()> {
        use regia_asset::content::{ChunkEntry, ContentManifest, Delivery};
        let root = regia_test_utils::unique_temp_dir("cli-android-packs");
        let pak_dir = root.join("build/paks/android");
        let gradle = root.join("build/android");
        std::fs::create_dir_all(&pak_dir)?;
        assert!(android_asset_packs(&pak_dir)?.is_empty(), "no manifest, no packs");
        let mut manifest = ContentManifest::new("1-1-x", "android");
        for (name, delivery) in [
            ("base", Delivery::Install),
            ("open-world", Delivery::OnDemand),
            ("intro", Delivery::FastFollow),
        ] {
            let pak = pak_dir.join(format!("{name}.fpak"));
            std::fs::write(&pak, name.as_bytes())?;
            manifest.chunks.push(ChunkEntry {
                name: name.into(),
                file: format!("{name}.fpak"),
                size: name.len() as u64,
                // Staging checks each pak against its entry (ADR-7413).
                hash: regia_asset::content::hash_file(&pak)?,
                delivery,
                requires: Vec::new(),
                overrides: false,
                mount_order: 0,
                assets: 1,
                blocks: Vec::new(),
            });
        }
        manifest
            .write_to_dir(&pak_dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let packs = android_asset_packs(&pak_dir)?;
        assert_eq!(packs.len(), 2);
        stage_asset_packs(&pak_dir, &gradle, &packs)?;
        assert_eq!(
            std::fs::read(gradle.join("packs/open_world/src/main/assets/open-world.fpak"))?,
            b"open-world"
        );
        assert!(gradle.join("packs/intro/src/main/assets/intro.fpak").is_file());
        assert!(
            !gradle.join("packs/base").exists(),
            "install chunks stay in the base module"
        );
        // A pack pak that is not what its entry records is refused (deep-zai-assets finding 4).
        std::fs::write(pak_dir.join("intro.fpak"), b"INTRO")?;
        let error = stage_asset_packs(&pak_dir, &gradle, &packs)
            .err()
            .unwrap_or_else(|| panic!("a replaced pack pak was staged"));
        assert!(format!("{error:#}").contains("chunk_mismatch"), "{error:#}");
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    const PLATFORM_MANIFEST: &str = r#"
[project]
name = "Game"
id = "com.example.game"

[platforms.linux]
formats = ["tar", "appimage"]
render_profile = "desktop_ultra"

[platforms.windows]
render_profile = "DesktopHigh"

[platforms.android]
render_profile = "MobileLow"
"#;

    #[test]
    fn render_profile_per_platform_reaches_build_info() -> Result<()> {
        let manifest = Manifest::parse(PLATFORM_MANIFEST)?;
        let root = regia_test_utils::unique_temp_dir("render-profile-build-info");
        for (platform, expected) in [
            ("linux", "DesktopUltra"),
            ("windows", "DesktopHigh"),
            ("android", "MobileLow"),
        ] {
            let mut extra = serde_json::Map::new();
            insert_render_profile(&mut extra, &manifest, platform);
            let staging = root.join(platform);
            std::fs::create_dir_all(&staging)?;
            write_build_info(&staging, &root, &manifest, extra)?;
            let info: serde_json::Value =
                serde_json::from_slice(&std::fs::read(staging.join("build_info.json"))?)?;
            assert_eq!(
                info["render_profile"], expected,
                "{platform}: canonical preset name"
            );
        }
        // ADR-8844: the integrated-GPU floor by name, in any spelling, reaches the package as the
        // canonical tier name.
        let very_low = Manifest::parse(&PLATFORM_MANIFEST.replace("\"DesktopHigh\"", "\"very_low\""))?;
        let mut extra = serde_json::Map::new();
        insert_render_profile(&mut extra, &very_low, "windows");
        let staging = root.join("windows-very-low");
        std::fs::create_dir_all(&staging)?;
        write_build_info(&staging, &root, &very_low, extra)?;
        let info: serde_json::Value =
            serde_json::from_slice(&std::fs::read(staging.join("build_info.json"))?)?;
        assert_eq!(info["render_profile"], "VeryLow");
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn package_boot_config_reaches_linux_and_android_staging() -> Result<()> {
        let root = regia_test_utils::unique_temp_dir("package-boot-config");
        let project = root.join("project");
        std::fs::create_dir_all(project.join("assets/movies"))?;
        let text = r#"[project]
name = "Boot Package"
id = "org.regia.bootpackage"
[boot]
movies = ["movies/intro.webm"]
skippable_after = 2.5
[game]
pause = true
pause_screen = "none"
[platforms.linux]
render_profile = "DesktopLow"
[platforms.android]
render_profile = "MobileLow"
"#;
        std::fs::write(project.join("Regia.toml"), text)?;
        std::fs::write(project.join("assets/movies/intro.webm"), b"fixture")?;
        let manifest = Manifest::parse(text)?;
        for platform in ["linux", "android"] {
            let staging = root.join(platform);
            let assets = if platform == "android" {
                staging.clone()
            } else {
                staging.join("assets")
            };
            stage_raw_assets(&project, &assets, Profile::Dev, platform)?;
            let mut extra = serde_json::Map::new();
            insert_render_profile(&mut extra, &manifest, platform);
            write_build_info(&staging, &project, &manifest, extra)?;
            let info: serde_json::Value =
                serde_json::from_slice(&std::fs::read(staging.join("build_info.json"))?)?;
            assert_eq!(
                info["boot"]["movies"],
                serde_json::json!(["movies/intro.webm"]),
                "{platform}"
            );
            assert_eq!(info["boot"]["skippable_after"], 2.5, "{platform}");
            assert_eq!(info["game"]["pause"], true, "{platform}");
            assert_eq!(info["game"]["pause_screen"], "none", "{platform}");
            assert!(assets.join("movies/intro.webm").is_file());
            assert!(!staging.join("Regia.toml").exists());
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn no_render_profile_writes_no_key() -> Result<()> {
        let manifest = Manifest::parse("[project]\nname = \"G\"\nid = \"com.example.g\"\n")?;
        let mut extra = serde_json::Map::new();
        insert_render_profile(&mut extra, &manifest, "linux");
        assert!(extra.is_empty());
        Ok(())
    }

    #[test]
    fn unknown_render_profile_and_linux_format_fail_the_manifest() {
        let bad_profile = PLATFORM_MANIFEST.replace("MobileLow", "MobileLudicrous");
        let err = Manifest::parse(&bad_profile).expect_err("unknown preset");
        assert!(err.to_string().starts_with("render_profile_unknown:"), "{err}");
        // ADR-8844: a tier that is a row already has no second name.
        let low = PLATFORM_MANIFEST.replace("\"DesktopHigh\"", "\"Low\"");
        let err = Manifest::parse(&low).expect_err("Low is no profile name");
        assert!(err.to_string().starts_with("render_profile_unknown:"), "{err}");
        assert!(err.to_string().contains("VeryLow"), "{err}");
        let bad_format = PLATFORM_MANIFEST.replace("\"appimage\"", "\"flatpak\"");
        let err = Manifest::parse(&bad_format).expect_err("unknown format");
        assert!(err.to_string().starts_with("linux_format_unknown:"), "{err}");
    }

    #[test]
    fn linux_formats_come_from_the_flag_then_the_manifest_then_tar() -> Result<()> {
        let manifest = Manifest::parse(PLATFORM_MANIFEST)?;
        let plain = Manifest::parse("[project]\nname = \"G\"\nid = \"com.example.g\"\n")?;
        let mut args = parse(&["--platform", "linux"])?;
        assert_eq!(linux_formats(&args, &manifest)?, [Format::Tar, Format::AppImage]);
        assert_eq!(linux_formats(&args, &plain)?, [Format::Tar]);
        args.format = Some(Format::Dir);
        assert_eq!(linux_formats(&args, &manifest)?, [Format::Dir]);
        assert_eq!(
            parse(&["--platform", "linux", "--format", "appimage"])?.format,
            Some(Format::AppImage)
        );
        args.format = Some(Format::Zip);
        assert!(linux_formats(&args, &manifest).is_err());
        Ok(())
    }

    /// A package ships a transcoded sound as Ogg Vorbis under its own name, and the Android
    /// package uses the Android quality (ADR-3230); a project without the policy file copies.
    #[test]
    fn audio_cook_policy_transcodes_staged_sounds() -> Result<()> {
        let project = regia_test_utils::unique_temp_dir("package_audio_cook");
        std::fs::create_dir_all(project.join("assets"))?;
        let rate = 44_100u32;
        let samples: Vec<u8> = (0..rate as usize * 2)
            .flat_map(|i| {
                let t = i as f32 / rate as f32;
                let v = 0.4 * (t * 440.0 * std::f32::consts::TAU).sin()
                    + 0.1 * (t * 2_700.0 * std::f32::consts::TAU).sin();
                ((v * 32_767.0) as i16).to_le_bytes()
            })
            .collect();
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        for v in [16u32, 0x0001_0001, rate, rate * 2, 0x0010_0002] {
            wav.extend_from_slice(&v.to_le_bytes());
        }
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(&samples);
        std::fs::write(project.join("assets/door.wav"), &wav)?;
        let base = project.join("out");

        let plain = base.join("plain");
        stage_raw_assets(&project, &plain, Profile::Dev, "linux")?;
        assert_eq!(
            std::fs::read(plain.join("door.wav"))?,
            wav,
            "no policy: a plain copy"
        );

        std::fs::write(
            project.join("audio_cook.toml"),
            "mode = \"vorbis\"\nquality = 0.8\nandroid_quality = 0.0\n",
        )?;
        let (linux, android) = (base.join("linux"), base.join("android"));
        stage_raw_assets(&project, &linux, Profile::Dev, "linux")?;
        stage_raw_assets(&project, &android, Profile::Dev, "android")?;
        let (l, a) = (
            std::fs::read(linux.join("door.wav"))?,
            std::fs::read(android.join("door.wav"))?,
        );
        assert_eq!(&l[..4], b"OggS");
        assert!(l.len() < wav.len() / 3, "{} vs {}", l.len(), wav.len());
        assert!(
            a.len() < l.len(),
            "android quality is lower: {} vs {}",
            a.len(),
            l.len()
        );
        let _ = std::fs::remove_dir_all(&project);
        Ok(())
    }

    /// A named profile stages its closure with `stage_raw_files`: its package gets the media
    /// index too (without it the Android runtime cannot find `movies/` in the APK), listing the
    /// media of the closure only (corr-cli; adapted to the stamped index of d158785df).
    #[test]
    fn audit_profile_staging_writes_the_android_media_index() -> Result<()> {
        let project = regia_test_utils::unique_temp_dir("package_profile_media_index");
        std::fs::create_dir_all(project.join("assets/movies"))?;
        std::fs::write(project.join("assets/movies/intro.webm"), b"webm")?;
        std::fs::write(project.join("assets/movies/unused.webm"), b"other")?;
        let only: std::collections::BTreeSet<String> = ["movies/intro.webm".to_owned()].into();
        for platform in ["android", "linux"] {
            let dest = project.join(platform);
            stage_raw_files(&project, &dest, &only, Profile::Dev, platform)?;
            assert!(dest.join("movies/intro.webm").is_file());
            let index: regia::PackagedMediaIndex =
                ron::from_str(&std::fs::read_to_string(dest.join(regia::PACKAGED_MEDIA_INDEX))?)?;
            let entries: Vec<(String, u64, u64)> = index
                .files
                .iter()
                .map(|e| (e.path.clone(), e.len, e.hash))
                .collect();
            assert_eq!(
                entries,
                [(
                    "movies/intro.webm".to_string(),
                    4,
                    regia::packaged_media_stamp(b"webm")
                )],
                "{platform}: the closure's media, stamped"
            );
        }
        let _ = std::fs::remove_dir_all(&project);
        Ok(())
    }

    /// integ-v048e (ADR-5922, from the v048-merge-conflict-audit report): a named profile whose
    /// closure stages no media still gets an empty Android media index, the rule
    /// `stage_raw_assets` follows (ADR-5795). Without it a later version of the profile that drops
    /// its last clip ships no index, the launch takes the package for one built before the index
    /// and never prunes the clips an earlier version extracted. Desktop gets no empty index.
    #[test]
    fn profile_staging_writes_an_empty_android_media_index_without_media() -> Result<()> {
        let project = regia_test_utils::unique_temp_dir("package_profile_empty_media_index");
        std::fs::create_dir_all(project.join("assets"))?;
        std::fs::write(project.join("assets/note.txt"), b"note")?;
        let only: std::collections::BTreeSet<String> = ["note.txt".to_owned()].into();

        let linux = project.join("linux");
        let on_linux = stage_raw_files(&project, &linux, &only, Profile::Dev, "linux")?;
        assert!(linux.join("note.txt").is_file());
        assert!(
            !linux.join(regia::PACKAGED_MEDIA_INDEX).exists(),
            "desktop: no media, no index"
        );

        let android = project.join("android");
        let staged = stage_raw_files(&project, &android, &only, Profile::Dev, "android")?;
        assert_eq!(staged, on_linux + 1, "the index is one more staged file");
        let index: regia::PackagedMediaIndex = ron::from_str(&std::fs::read_to_string(
            android.join(regia::PACKAGED_MEDIA_INDEX),
        )?)?;
        assert_eq!(index.format_version, 1);
        assert!(index.files.is_empty(), "{:?}", index.files);
        let _ = std::fs::remove_dir_all(&project);
        Ok(())
    }

    /// A level variant ships flattened under its own name (ADR-7823): the package holds the
    /// resolved level as a scene file with the variant's sidecar, and the base ships as itself.
    #[test]
    fn a_level_variant_ships_as_its_resolved_level() -> Result<()> {
        let project = regia_test_utils::unique_temp_dir("package_level_variant");
        let assets = project.join("assets");
        std::fs::create_dir_all(&assets)?;
        let t = "regia_scene::transform::Transform";
        let (a, b) = (
            regia_core::id::Guid::from_u128(1),
            regia_core::id::Guid::from_u128(2),
        );
        std::fs::write(
            assets.join("arena.level.ron"),
            format!(
                r#"SceneFile(format_version: {v}, entities: [(guid: "{a}", components: {{ "{t}": (translation: (0.0, 0.0, 0.0)) }}), (guid: "{b}", components: {{}})])"#,
                v = regia_scene::format::SCENE_FORMAT_VERSION
            ),
        )?;
        let meta = |path: &Path, kind: &str, id: u128| -> Result<()> {
            let sidecar = regia_asset::MetaSidecar::new(
                kind,
                regia_asset::AssetId(regia_core::id::Guid::from_u128(id)),
            );
            std::fs::write(regia_asset::meta_path_for(path), sidecar.to_ron_string()?)?;
            Ok(())
        };
        meta(&assets.join("arena.level.ron"), "level", 0xb0)?;
        let variant = assets.join("arena_night.levelvariant.ron");
        std::fs::write(
            &variant,
            format!(
                r#"LevelVariant(format_version: 1, base: "asset:{base}", overrides: [(target: "{a}", component: "{t}", path: ".translation", value: (5.0, 0.0, 0.0))], removed_entities: ["{b}"])"#,
                base = regia_core::id::Guid::from_u128(0xb0)
            ),
        )?;
        meta(
            &variant,
            regia_scene::level_variant::LEVEL_VARIANT_META_KIND,
            0xb1,
        )?;

        let dest = project.join("package/assets");
        assert_eq!(copy_raw_assets(&project, &dest)?, 2);
        let shipped = std::fs::read_to_string(dest.join("arena_night.levelvariant.ron"))?;
        assert!(shipped.starts_with("SceneFile("), "{shipped}");
        let scene = regia_scene::format::SceneFile::from_ron_str(&shipped)?;
        assert_eq!(scene.entities.len(), 1);
        assert_eq!(
            scene.entities[0].components[t].get_ron(),
            "(translation: (5.0, 0.0, 0.0))"
        );
        assert!(regia_asset::meta_path_for(&dest.join("arena_night.levelvariant.ron")).is_file());

        // A variant whose base is gone stops the package with the reason.
        std::fs::remove_file(assets.join("arena.level.ron"))?;
        std::fs::remove_file(regia_asset::meta_path_for(&assets.join("arena.level.ron")))?;
        let err = copy_raw_assets(&project, &project.join("package2/assets")).expect_err("no base");
        assert!(format!("{err:#}").contains("base_missing"), "{err:#}");
        let _ = std::fs::remove_dir_all(&project);
        Ok(())
    }

    #[test]
    fn shipping_gate_rejects_planted_string_while_dev_remains_usable() -> Result<()> {
        let root = regia_test_utils::unique_temp_dir("package-hygiene-shipping-hook");
        let stage = root.join("staging");
        std::fs::create_dir_all(&stage)?;
        regia_test_utils::write_atomic(
            &root.join("Regia.toml"),
            b"[package_hygiene]\nforbidden_strings=['--developer-console']\n",
        )?;
        regia_test_utils::write_atomic(&stage.join("player"), b"\0--developer-consoleNEXT")?;
        assert!(check_package_hygiene(&root, &stage, Profile::Dev).is_ok());
        let error = check_package_hygiene(&root, &stage, Profile::Shipping).expect_err("shipping rejects");
        assert_eq!(
            error
                .downcast_ref::<regia_cli::package_hygiene::AuditError>()
                .expect("typed error")
                .code,
            "package_hygiene_failed"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// ADR-8453: `--prepare-scene` lands in `build_info.json` as `prepare_maps`, in order, with
    /// the name `regia prepare --scene` gives the map; a level that is not there fails the package.
    #[test]
    fn prepare_scenes_become_the_prepare_maps_of_build_info() {
        let root = regia_test_utils::unique_temp_dir("package-prepare-maps");
        std::fs::create_dir_all(root.join("levels/canyon.level.d")).expect("dirs");
        std::fs::write(root.join("levels/forest.level.ron"), "()").expect("level");
        let maps = prepare_maps(
            &root,
            &[
                PathBuf::from("levels/forest.level.ron"),
                root.join("levels/canyon.level.d"),
            ],
        )
        .expect("maps");
        assert_eq!(
            maps,
            serde_json::json!([
                { "name": "levels/forest", "path": "levels/forest.level.ron" },
                { "name": "levels/canyon", "path": "levels/canyon.level.d" },
            ])
        );
        assert_eq!(prepare_maps(&root, &[]).expect("none"), serde_json::json!([]));
        assert!(prepare_maps(&root, &[PathBuf::from("levels/none.level.ron")]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn final_hook_writes_a_shipping_receipt_and_dev_removes_stale_ones() -> Result<()> {
        use regia_cli::package_hygiene::receipt_path;
        let root = regia_test_utils::unique_temp_dir("package-final-artifact-hook");
        std::fs::create_dir_all(&root)?;
        regia_test_utils::write_atomic(
            &root.join("Regia.toml"),
            b"[package_hygiene]\nforbidden_strings=['--developer-console']\n",
        )?;
        let artifact = root.join("game.bin");
        regia_test_utils::write_atomic(&artifact, b"clean shipped bytes")?;
        final_artifact_hook(&root, &artifact, Profile::Shipping, &[])?;
        let sidecar = receipt_path(&artifact);
        assert!(sidecar.is_file());
        let json = final_audit_json(&root, &artifact);
        assert_eq!(json["verified"], true);
        assert_eq!(json["artifact_kind"], "file");
        // A dev package never keeps an earlier shipping receipt beside a new artifact.
        final_artifact_hook(&root, &artifact, Profile::Dev, &[])?;
        assert!(!sidecar.exists());
        assert!(final_audit_json(&root, &artifact).is_null());
        // A rejected final artifact keeps no receipt and is not modified.
        regia_test_utils::write_atomic(&artifact, b"late --developer-console")?;
        let error = final_artifact_hook(&root, &artifact, Profile::Shipping, &[]).expect_err("rejected");
        assert_eq!(
            error
                .downcast_ref::<regia_cli::package_hygiene::AuditError>()
                .expect("typed error")
                .code,
            "package_hygiene_failed"
        );
        assert!(!sidecar.exists());
        assert_eq!(std::fs::read(&artifact)?, b"late --developer-console");
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
