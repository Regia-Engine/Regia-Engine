// SPDX-License-Identifier: MPL-2.0
//! `regia doctor` — check toolchains and print fixes
//! (`docs/specs/21-tooling-and-packaging.md` § 1, `AGENTS.md` § 5, ADR-1379).
//!
//! Every check is a [`Check`]: the section it belongs to (`rust`, `linux`, `windows`, `deck`,
//! `macos`, `android`, `gpu`), what was found, a status, a stable snake_case `code` and, for a
//! warning or a failure, the command that fixes it. `--json` prints the checks as one JSON
//! document (schema 1, `book/agents/doctor.md`) instead of the text report. A failed check makes
//! the command exit non-zero, so `regia doctor && regia package` stops.
//!
//! The checks that run commands go through a probe (the real system, or a fake in the tests), so
//! each platform's checks are tested on any host.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use clap::Args;
use regia_render::hwrt::{AccelLimits, HwrtReason, HwrtRequest};
use serde_json::{Value, json};

use crate::android::toolchain;
use crate::process;

/// Version of the `--json` document.
pub const DOCTOR_SCHEMA: u32 = 1;

/// The platforms `--platform` accepts.
pub const PLATFORMS: &[&str] = &["linux", "windows", "android", "macos", "deck", "gpu", "all"];

/// The oldest zig `cargo-zigbuild` builds the Deck bundle with (the CI pin, `.github/workflows/ci.yml`).
const MIN_ZIG: [u32; 2] = [0, 14];

/// Arguments of `regia doctor`.
#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    /// Check a specific platform (`linux`, `windows`, `android`, `macos`, `deck`, `all`), or `gpu`
    /// for the GPU ray-tracing probe alone. Omit to check all that apply on this host (Android problems are warnings then, not failures).
    #[arg(long)]
    pub platform: Option<String>,

    /// Project directory (looks for rust-toolchain.toml to pick the right Rust version).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,

    /// Verify the integrity (whole-file content hash) of every `.fpak` in this directory, e.g. a
    /// shipped game's `content/` folder. Only this check runs when given.
    #[arg(long, value_name = "DIR")]
    pub verify_paks: Option<PathBuf>,

    /// Print the checks (or, with `--verify-paks`, the pak results) as JSON (`book/agents/doctor.md`) instead of the text report.
    #[arg(long)]
    pub json: bool,
}

/// How one check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// Found and usable.
    Ok,
    /// Missing or old, but only some workflows need it.
    Warn,
    /// Missing: the workflows of this platform cannot run.
    Fail,
    /// A fact, never a problem (the GPU ray-tracing probe, testing-only platforms).
    Info,
}

impl CheckStatus {
    /// `ok`, `warn`, `fail` or `info`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Info => "info",
        }
    }
}

/// One doctor check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// `rust`, `linux`, `windows`, `deck`, `macos`, `android` or `gpu`.
    pub section: &'static str,
    /// The tool or item checked (`rustc`, `cargo-xwin`, `zig`, ...).
    pub name: String,
    /// The outcome.
    pub status: CheckStatus,
    /// Stable snake_case code: `ok`, or what is wrong (`cargo_xwin_missing`, ...). The codes are
    /// listed in `book/agents/doctor.md`.
    pub code: &'static str,
    /// What was found (a version line, a path), or what is missing.
    pub detail: String,
    /// The command or step that fixes a warning or failure.
    pub fix: Option<String>,
}

impl Check {
    fn ok(section: &'static str, name: &str, detail: impl Into<String>) -> Self {
        Self {
            section,
            name: name.to_string(),
            status: CheckStatus::Ok,
            code: "ok",
            detail: detail.into(),
            fix: None,
        }
    }

    fn info(section: &'static str, name: &str, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            section,
            name: name.to_string(),
            status: CheckStatus::Info,
            code,
            detail: detail.into(),
            fix: None,
        }
    }

    fn problem(
        section: &'static str,
        name: &str,
        status: CheckStatus,
        code: &'static str,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            section,
            name: name.to_string(),
            status,
            code,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    /// The check as a JSON object (`book/agents/doctor.md`).
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "section": self.section,
            "name": self.name,
            "status": self.status.label(),
            "code": self.code,
            "detail": self.detail,
            "fix": self.fix,
        })
    }
}

/// What the checks may ask of the machine. The real implementation runs commands and reads the
/// file system; the tests use a fake.
pub(crate) trait Probe {
    /// The trimmed stdout of `program args` when it ran and exited 0.
    fn output(&self, program: &str, args: &[&str]) -> Option<String>;
    /// `std::env::consts::OS` of the host.
    fn host_os(&self) -> &str;
    /// Whether `path` is a file.
    fn is_file(&self, path: &Path) -> bool;
    /// Whether `path` is a directory with at least one entry.
    fn non_empty_dir(&self, path: &Path) -> bool;
    /// An environment variable.
    fn env(&self, key: &str) -> Option<String>;
    /// Free bytes on the volume holding `path`, when the host can tell.
    fn free_bytes(&self, path: &Path) -> Option<u64>;
}

/// The real machine. Commands run in the project directory, so a `rust-toolchain.toml` there picks
/// the toolchain whose version and targets are reported.
struct System {
    cwd: Option<PathBuf>,
}

impl Probe for System {
    fn output(&self, program: &str, args: &[&str]) -> Option<String> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        process::try_output(&mut command).map(|s| s.trim().to_string())
    }

    fn host_os(&self) -> &str {
        std::env::consts::OS
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn non_empty_dir(&self, path: &Path) -> bool {
        std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_some())
    }

    fn env(&self, key: &str) -> Option<String> {
        regia_core::brand::env_var(key).ok().filter(|v| !v.is_empty())
    }

    fn free_bytes(&self, path: &Path) -> Option<u64> {
        if cfg!(windows) {
            let drive = path
                .canonicalize()
                .ok()?
                .to_string_lossy()
                .trim_start_matches(r"\\?\")
                .chars()
                .next()?;
            let script = format!("(Get-PSDrive -Name {drive}).Free");
            let out = self.output("powershell", &["-NoProfile", "-Command", &script])?;
            out.trim().parse().ok()
        } else {
            let target = path.to_string_lossy();
            let out = self.output("df", &["-Pk", &target])?;
            parse_df_free_bytes(&out)
        }
    }
}

/// Free bytes from `df -Pk` output: the "Available" column of the last row, in KiB.
fn parse_df_free_bytes(text: &str) -> Option<u64> {
    let row = text.lines().filter(|l| !l.trim().is_empty()).nth(1)?;
    let kib: u64 = row.split_whitespace().nth(3)?.parse().ok()?;
    kib.checked_mul(1024)
}

/// Below this many free bytes a build is likely to fail part way (`disk_full`).
const DISK_FAIL_BYTES: u64 = 2 << 30;
/// Below this many free bytes a cold build or an Android package may not fit (`disk_low`).
const DISK_WARN_BYTES: u64 = 15 << 30;
/// The oldest NDK major version Android builds accept (`AGENTS.md` § 5: r27+).
const MIN_NDK_MAJOR: u32 = 27;

/// Free disk space where builds write (`dir`, the project): a cold build of the engine and an
/// Android package take many GiB.
fn check_disk(probe: &dyn Probe, section: &'static str, dir: &Path) -> Check {
    let gib = |b: u64| format!("{:.1} GiB", b as f64 / f64::from(1u32 << 30));
    match probe.free_bytes(dir) {
        None => Check::info(
            section,
            "disk",
            "ok",
            format!("free space of {} could not be read on this host", dir.display()),
        ),
        Some(b) if b < DISK_FAIL_BYTES => Check::problem(
            section,
            "disk",
            CheckStatus::Fail,
            "disk_full",
            format!("only {} free where {} builds", gib(b), dir.display()),
            "free space: `cargo clean`, or set CARGO_TARGET_DIR to a larger volume (need 15 GiB or more)",
        ),
        Some(b) if b < DISK_WARN_BYTES => Check::problem(
            section,
            "disk",
            CheckStatus::Warn,
            "disk_low",
            format!(
                "{} free where {} builds; a cold build wants 15 GiB",
                gib(b),
                dir.display()
            ),
            "free space with `cargo clean`, or set CARGO_TARGET_DIR to a larger volume",
        ),
        Some(b) => Check::ok(section, "disk", format!("{} free", gib(b))),
    }
}

/// The major version of an NDK revision string (`28.0.13004108` → 28).
fn ndk_major(version: &str) -> Option<u32> {
    version.split('.').next()?.trim().parse().ok()
}

/// The SDK parts `sdkmanager` installs next to the NDK: a platform and build tools.
fn check_android_sdk_parts(probe: &dyn Probe, sdk: &Path) -> Vec<Check> {
    let s = "android";
    let mut out = Vec::new();
    for (dir, name, code, package) in [
        (
            "platforms",
            "sdk-platform",
            "android_platform_missing",
            "platforms;android-35",
        ),
        (
            "build-tools",
            "sdk-build-tools",
            "android_build_tools_missing",
            "build-tools;35.0.0",
        ),
    ] {
        let path = sdk.join(dir);
        out.push(if probe.non_empty_dir(&path) {
            Check::ok(s, name, path.display().to_string())
        } else {
            Check::problem(
                s,
                name,
                CheckStatus::Fail,
                code,
                format!("{} is missing or empty", path.display()),
                format!("sdkmanager \"{package}\""),
            )
        });
    }
    out
}

/// A section of the report, in print order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Rust,
    Linux,
    Windows,
    Deck,
    Macos,
    Android,
    Gpu,
}

impl Section {
    fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::Deck => "deck",
            Self::Macos => "macos",
            Self::Android => "android",
            Self::Gpu => "gpu",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Linux => "Linux desktop (Vulkan)",
            Self::Windows => "Windows",
            Self::Deck => "Steam Deck bundle",
            Self::Macos => "macOS (testing only)",
            Self::Android => "Android",
            Self::Gpu => "GPU ray tracing",
        }
    }
}

/// The sections `--platform` selects on a `host_os` machine.
///
/// # Errors
/// An unknown platform name: the message lists the valid ones.
fn sections_for(platform: Option<&str>, host_os: &str) -> Result<Vec<Section>> {
    let host_desktop = match host_os {
        "windows" => Section::Windows,
        "macos" => Section::Macos,
        _ => Section::Linux,
    };
    let mut sections = match platform {
        None => vec![Section::Rust, host_desktop, Section::Gpu, Section::Android],
        Some("linux") => vec![Section::Rust, Section::Linux, Section::Gpu],
        Some("windows") => vec![Section::Rust, Section::Windows, Section::Gpu],
        Some("macos") => vec![Section::Rust, Section::Macos, Section::Gpu],
        Some("deck") => vec![Section::Rust, Section::Deck],
        Some("android") => vec![Section::Rust, Section::Android],
        Some("gpu") => vec![Section::Gpu],
        Some("all") => vec![
            Section::Rust,
            host_desktop,
            Section::Windows,
            Section::Deck,
            Section::Macos,
            Section::Android,
            Section::Gpu,
        ],
        Some(other) => {
            return Err(crate::commands::assets_validate::UsageError(format!(
                "unknown --platform {other:?}: expected one of {}",
                PLATFORMS.join(", ")
            ))
            .into());
        }
    };
    sections.sort();
    sections.dedup();
    Ok(sections)
}

/// Run `regia doctor`.
///
/// # Errors
/// An unknown `--platform`, or any failed check (after the report is printed).
pub fn run(args: &DoctorArgs) -> Result<()> {
    let probe = System {
        cwd: Some(args.project.clone()).filter(|p| p.is_dir()),
    };
    run_with(args, &probe, check_ray_tracing)
}

/// [`run`] with the machine probe and the GPU checks injected, so the wiring is tested
/// hermetically (the tests use a fake probe; the old smoke test paid for real subprocesses and
/// a Vulkan instance in every `cargo test` run, and asserted nothing on its real-machine half).
fn run_with(args: &DoctorArgs, probe: &dyn Probe, gpu_checks: fn() -> Vec<Check>) -> Result<()> {
    if let Some(dir) = &args.verify_paks {
        return verify_paks(dir, args.json);
    }
    let sections = sections_for(args.platform.as_deref(), probe.host_os())?;
    let checks = collect_checks(args, probe, gpu_checks, &sections);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report_json(&checks, probe.host_os(), &sections))?
        );
    } else {
        print!("{}", report_text(&checks, &sections));
    }
    finish(&checks)
}

/// The checks of one run: one batch per section — the disk check is added to Rust and the GPU
/// probe injected — with Android failures softened to warnings when no `--platform` was given.
fn collect_checks(
    args: &DoctorArgs,
    probe: &dyn Probe,
    gpu_checks: fn() -> Vec<Check>,
    sections: &[Section],
) -> Vec<Check> {
    let mut checks = Vec::new();
    for section in sections {
        checks.extend(match section {
            Section::Android => check_android(probe),
            Section::Gpu => gpu_checks(),
            Section::Rust => {
                let mut rust = checks_for(Section::Rust, probe);
                rust.push(check_disk(probe, "rust", &args.project));
                rust
            }
            other => checks_for(*other, probe),
        });
    }
    if args.platform.is_none() {
        soften_android(&mut checks);
    }
    checks
}

/// With no `--platform`, Android is checked as a courtesy, not a gate: its failures become
/// warnings (codes and fixes unchanged) so `regia doctor && regia package` still works for a
/// desktop game on a machine without the Android SDK. `--platform android` or `all` keeps them
/// as failures.
fn soften_android(checks: &mut [Check]) {
    for c in checks
        .iter_mut()
        .filter(|c| c.section == "android" && c.status == CheckStatus::Fail)
    {
        c.status = CheckStatus::Warn;
    }
}

/// The `--json` document.
fn report_json(checks: &[Check], host_os: &str, sections: &[Section]) -> Value {
    let failed: Vec<&str> = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .map(|c| c.code)
        .collect();
    json!({
        "schema": DOCTOR_SCHEMA,
        "kind": "regia_doctor",
        "host": host_os,
        "ok": failed.is_empty(),
        "sections": sections.iter().map(|s| s.name()).collect::<Vec<_>>(),
        "failed": failed,
        "warnings": checks.iter().filter(|c| c.status == CheckStatus::Warn).count(),
        "checks": checks.iter().map(Check::to_json).collect::<Vec<_>>(),
    })
}

/// The text report: one block per section, one line per check, the fix under each problem.
fn report_text(checks: &[Check], sections: &[Section]) -> String {
    let mut out = String::from("Regia Engine Doctor\n==============\n");
    for section in sections {
        out.push_str(&format!("\n[{}]\n", section.title()));
        for c in checks.iter().filter(|c| c.section == section.name()) {
            let tag = match c.status {
                CheckStatus::Ok => "ok  ",
                CheckStatus::Warn => "WARN",
                CheckStatus::Fail => "FAIL",
                CheckStatus::Info => "    ",
            };
            out.push_str(&format!("  {tag} {}: {}\n", c.name, c.detail));
            if let Some(fix) = &c.fix {
                out.push_str(&format!("       fix: {fix}\n"));
            }
        }
    }
    out.push('\n');
    let failed = checks.iter().filter(|c| c.status == CheckStatus::Fail).count();
    let warned = checks.iter().filter(|c| c.status == CheckStatus::Warn).count();
    if failed == 0 {
        out.push_str(&format!("All checks passed ({warned} warning(s)).\n"));
    } else {
        out.push_str(&format!(
            "{failed} check(s) failed, {warned} warning(s). Run the `fix:` commands above, then `regia doctor` again.\n"
        ));
    }
    out
}

/// A failed check is an error so `regia doctor && regia package` stops; the error names the codes.
fn finish(checks: &[Check]) -> Result<()> {
    let failed: Vec<&str> = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .map(|c| c.code)
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        bail!("{} doctor check(s) failed: {}", failed.len(), failed.join(", "))
    }
}

/// One pak's outcome in `--verify-paks`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PakRow {
    path: PathBuf,
    /// `ok`, `unhashed` or `corrupt`.
    status: &'static str,
    detail: Option<String>,
}

impl PakRow {
    fn to_json(&self) -> Value {
        json!({
            "path": self.path.display().to_string(),
            "status": self.status,
            "detail": self.detail,
        })
    }
}

/// Hash every `.fpak` in `dir` (sorted by path).
fn pak_rows(dir: &Path) -> Result<Vec<PakRow>> {
    let mut paks: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "fpak"))
        .collect();
    paks.sort();
    if paks.is_empty() {
        bail!("no .fpak files in {}", dir.display());
    }
    Ok(paks
        .into_iter()
        .map(|path| match regia_asset::verify_pak_file(&path) {
            Ok(regia_asset::PakIntegrity::Ok) => PakRow {
                path,
                status: "ok",
                detail: None,
            },
            Ok(regia_asset::PakIntegrity::Unhashed) => PakRow {
                path,
                status: "unhashed",
                detail: Some("no content hash to check".to_string()),
            },
            Ok(regia_asset::PakIntegrity::Mismatch { expected, actual }) => PakRow {
                path,
                status: "corrupt",
                detail: Some(format!("expected {expected}, got {actual}")),
            },
            Err(e) => PakRow {
                path,
                status: "corrupt",
                detail: Some(e.to_string()),
            },
        })
        .collect())
}

/// The `--verify-paks --json` document.
fn pak_report_json(rows: &[PakRow]) -> Value {
    let corrupt = rows.iter().filter(|r| r.status == "corrupt").count();
    json!({
        "schema": DOCTOR_SCHEMA,
        "kind": "regia_doctor_paks",
        "ok": corrupt == 0,
        "corrupt": corrupt,
        "paks": rows.iter().map(PakRow::to_json).collect::<Vec<_>>(),
    })
}

/// The `--verify-paks` text report.
fn pak_report_text(rows: &[PakRow]) -> String {
    let mut out = String::new();
    for r in rows {
        let line = match (r.status, &r.detail) {
            ("ok", _) => format!("  [ok]   {}\n", r.path.display()),
            ("unhashed", _) => format!("  [--]   {} (no content hash to check)\n", r.path.display()),
            (_, detail) => format!(
                "  [FAIL] {}: {}\n",
                r.path.display(),
                detail.as_deref().unwrap_or("corrupt")
            ),
        };
        out.push_str(&line);
    }
    out
}

/// `regia doctor --verify-paks DIR`: hashes every pak in `dir`; an error when one is corrupt.
fn verify_paks(dir: &Path, json_out: bool) -> Result<()> {
    let rows = pak_rows(dir)?;
    let bad = rows.iter().filter(|r| r.status == "corrupt").count();
    if json_out {
        println!("{}", serde_json::to_string_pretty(&pak_report_json(&rows))?);
    } else {
        print!("{}", pak_report_text(&rows));
    }
    if bad > 0 {
        bail!(
            "{bad} of {} pak(s) failed the integrity check: verify the game files or reinstall",
            rows.len()
        );
    }
    if !json_out {
        println!("All {} pak(s) intact.", rows.len());
    }
    Ok(())
}

/// The checks of a section that only run commands and read files (every section but Android and
/// the GPU probe).
fn checks_for(section: Section, probe: &dyn Probe) -> Vec<Check> {
    match section {
        Section::Rust => check_rust(probe),
        Section::Linux => check_linux(probe),
        Section::Windows => check_windows(probe),
        Section::Deck => check_deck(probe),
        Section::Macos => check_macos(probe),
        Section::Android | Section::Gpu => Vec::new(),
    }
}

/// The first line of a tool's version output.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

fn check_rust(probe: &dyn Probe) -> Vec<Check> {
    let s = "rust";
    let mut out = Vec::new();
    for tool in ["rustc", "cargo"] {
        out.push(match probe.output(tool, &["--version"]) {
            Some(v) => Check::ok(s, tool, first_line(&v)),
            None => Check::problem(
                s,
                tool,
                CheckStatus::Fail,
                if tool == "rustc" {
                    "rustc_missing"
                } else {
                    "cargo_missing"
                },
                format!("`{tool} --version` did not run"),
                "install Rust from https://rustup.rs (a distro rust package works for desktop builds)",
            ),
        });
    }
    out.push(match probe.output("rustup", &["--version"]) {
        Some(v) => Check::ok(s, "rustup", first_line(&v)),
        None => Check::problem(
            s,
            "rustup",
            CheckStatus::Warn,
            "rustup_missing",
            "not found; distro rustc builds the desktop targets, but Android and Windows cross builds need rustup-managed targets",
            "install rustup from https://rustup.rs",
        ),
    });
    out
}

/// `spirv-val`, which the shader validation tests use (`regia_matgraph`, `shader_check`).
fn check_spirv_val(section: &'static str, probe: &dyn Probe) -> Check {
    match probe.output("spirv-val", &["--version"]) {
        Some(v) => Check::ok(section, "spirv-val", first_line(&v)),
        None => Check::problem(
            section,
            "spirv-val",
            CheckStatus::Warn,
            "spirv_val_missing",
            "not found; the SPIR-V validation tests (regia_matgraph, shader_check) fail without it",
            if section == "macos" {
                "brew install spirv-tools"
            } else {
                "install spirv-tools (Arch: spirv-tools, Debian/Ubuntu: spirv-tools)"
            },
        ),
    }
}

fn check_linux(probe: &dyn Probe) -> Vec<Check> {
    let s = "linux";
    let vulkan = match probe.output("vulkaninfo", &["--summary"]) {
        Some(out) => {
            let devices: Vec<String> = out
                .lines()
                .filter(|l| l.contains("deviceName"))
                .map(|l| l.split('=').nth(1).unwrap_or(l).trim().to_string())
                .collect();
            if devices.is_empty() {
                Check::problem(
                    s,
                    "vulkan",
                    CheckStatus::Fail,
                    "vulkan_no_device",
                    "vulkaninfo ran but lists no device",
                    "install your GPU's Vulkan driver (Mesa: vulkan-radeon / vulkan-intel; NVIDIA: the proprietary driver); for tests, mesa's lavapipe (vulkan-swrast) also works",
                )
            } else {
                Check::ok(s, "vulkan", devices.join(", "))
            }
        }
        None => {
            let icd = ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"]
                .into_iter()
                .any(|d| probe.non_empty_dir(Path::new(d)));
            if icd {
                Check::problem(
                    s,
                    "vulkan",
                    CheckStatus::Warn,
                    "vulkaninfo_missing",
                    "a Vulkan driver manifest is installed, but vulkaninfo is not, so the device cannot be listed",
                    "install vulkan-tools to see the devices",
                )
            } else {
                Check::problem(
                    s,
                    "vulkan",
                    CheckStatus::Fail,
                    "vulkan_missing",
                    "no vulkaninfo and no driver manifest in /usr/share/vulkan/icd.d or /etc/vulkan/icd.d",
                    "install your GPU's Vulkan driver and vulkan-tools",
                )
            }
        }
    };
    let mut out = vec![vulkan, check_spirv_val(s, probe)];
    out.extend(check_editor_launcher(s, "linux", probe));
    out
}

/// The Regia Editor's entry in this host's app launcher (ADR-20000), on the host's own section
/// only. A missing launcher is a fact (`info`), never a problem; `None` when the per-user
/// directory is unknown.
fn check_editor_launcher(section: &'static str, host: &str, probe: &dyn Probe) -> Option<Check> {
    use super::editor_launcher::{Desktop, UserDirs, entry_path};
    if probe.host_os() != host {
        return None;
    }
    let desktop = match host {
        "linux" => Desktop::Linux,
        "windows" => Desktop::Windows,
        "macos" => Desktop::Mac,
        _ => return None,
    };
    let entry = entry_path(desktop, &UserDirs::from_env(|key| probe.env(key)))?;
    let marker = if desktop == Desktop::Mac {
        entry.join("Contents").join("Info.plist")
    } else {
        entry.clone()
    };
    Some(if probe.is_file(&marker) {
        Check::ok(section, "editor-launcher", entry.display().to_string())
    } else {
        Check::info(
            section,
            "editor-launcher",
            "editor_launcher_missing",
            format!(
                "{} not found: the Regia Editor is not in this desktop's app launcher; add it with `regia editor install-launcher`",
                entry.display()
            ),
        )
    })
}

fn check_windows(probe: &dyn Probe) -> Vec<Check> {
    let s = "windows";
    if probe.host_os() == "windows" {
        let system = probe
            .env("SystemRoot")
            .unwrap_or_else(|| r"C:\Windows".to_string());
        let dll = Path::new(&system).join("System32").join("vulkan-1.dll");
        let loader = if probe.is_file(&dll) {
            Check::ok(s, "vulkan", format!("Vulkan loader {}", dll.display()))
        } else {
            Check::problem(
                s,
                "vulkan",
                CheckStatus::Warn,
                "vulkan_loader_missing",
                format!(
                    "{} not found: Regia Engine falls back to Direct3D 12 (ADR-201)",
                    dll.display()
                ),
                "update the GPU driver (it installs the Vulkan loader)",
            )
        };
        let mut out = vec![
            loader,
            Check::info(
                s,
                "direct3d12",
                "ok",
                "part of Windows 10 and 11; the fallback backend",
            ),
        ];
        out.extend(check_editor_launcher(s, "windows", probe));
        return out;
    }
    // Cross builds from Linux or macOS: cargo-xwin, the MSVC target, clang-cl for the C deps.
    let mut out = Vec::new();
    out.push(match probe.output("cargo", &["xwin", "--version"]) {
        Some(v) => Check::ok(s, "cargo-xwin", first_line(&v)),
        None => Check::problem(
            s,
            "cargo-xwin",
            CheckStatus::Fail,
            "cargo_xwin_missing",
            "not found; Windows packages and bundles cross-build with it (`regia package --platform windows`, `cargo xtask dist --target windows`)",
            "cargo install cargo-xwin --locked",
        ),
    });
    out.push(rustup_target(
        s,
        probe,
        "x86_64-pc-windows-msvc",
        "windows_target_missing",
    ));
    let clang = ["clang-cl", "lld-link"]
        .into_iter()
        .filter(|tool| probe.output(tool, &["--version"]).is_none())
        .collect::<Vec<_>>();
    out.push(if clang.is_empty() {
        Check::ok(s, "clang-cl", "clang-cl and lld-link found")
    } else {
        Check::problem(
            s,
            "clang-cl",
            CheckStatus::Warn,
            "clang_cl_missing",
            format!(
                "{} not found; cargo-xwin compiles the C and C++ dependencies (astcenc, ISPC kernels) with them",
                clang.join(" and ")
            ),
            "install clang, lld and llvm",
        )
    });
    out
}

/// A `rustup target list --installed` check for `triple`.
fn rustup_target(section: &'static str, probe: &dyn Probe, triple: &str, code: &'static str) -> Check {
    match probe.output("rustup", &["target", "list", "--installed"]) {
        Some(installed) if installed.lines().any(|l| l.trim() == triple) => {
            Check::ok(section, triple, "rustup target installed")
        }
        Some(_) => Check::problem(
            section,
            triple,
            CheckStatus::Fail,
            code,
            "rustup target not installed",
            format!("rustup target add {triple}"),
        ),
        None => Check::problem(
            section,
            triple,
            CheckStatus::Fail,
            "rustup_missing",
            "cannot check the target without rustup",
            format!("install rustup from https://rustup.rs, then `rustup target add {triple}`"),
        ),
    }
}

/// Parses the leading `major.minor` of a version string (`0.14.1` → `[0, 14]`).
fn major_minor(text: &str) -> Option<[u32; 2]> {
    let mut parts = text.trim().split(['.', '-', ' ']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some([major, minor])
}

fn check_deck(probe: &dyn Probe) -> Vec<Check> {
    let s = "deck";
    if probe.host_os() != "linux" {
        return vec![Check::problem(
            s,
            "host",
            CheckStatus::Fail,
            "deck_needs_linux",
            format!(
                "the Deck bundle builds on Linux (cargo-zigbuild against glibc 2.31); this host is {}",
                probe.host_os()
            ),
            "build on a Linux machine, or bundle prebuilt binaries with `cargo xtask dist --target deck --from <dir>`",
        )];
    }
    let mut out = Vec::new();
    out.push(match probe.output("cargo", &["zigbuild", "--version"]) {
        Some(v) => Check::ok(s, "cargo-zigbuild", first_line(&v)),
        None => Check::problem(
            s,
            "cargo-zigbuild",
            CheckStatus::Fail,
            "cargo_zigbuild_missing",
            "not found; Deck and Steam Linux Runtime builds link against glibc 2.31 with it",
            "cargo install cargo-zigbuild --locked",
        ),
    });
    let want = format!("{}.{}", MIN_ZIG[0], MIN_ZIG[1]);
    out.push(match probe.output("zig", &["version"]) {
        Some(v) => match major_minor(&v) {
            Some(found) if found >= MIN_ZIG => Check::ok(s, "zig", first_line(&v)),
            _ => Check::problem(
                s,
                "zig",
                CheckStatus::Fail,
                "zig_too_old",
                format!("zig {} is older than {want}", first_line(&v)),
                format!("install zig {want} or newer (https://ziglang.org/download)"),
            ),
        },
        None => Check::problem(
            s,
            "zig",
            CheckStatus::Fail,
            "zig_missing",
            "not found on PATH; cargo-zigbuild needs it",
            format!("install zig {want} or newer (https://ziglang.org/download)"),
        ),
    });
    out
}

fn check_macos(probe: &dyn Probe) -> Vec<Check> {
    let s = "macos";
    if probe.host_os() != "macos" {
        return vec![Check::info(
            s,
            "host",
            "macos_testing_only",
            "macOS builds and tests run on the Mac only (testing, never shipped; ADR-264): run `regia doctor --platform macos` there",
        )];
    }
    let xcode = match probe.output("xcode-select", &["-p"]) {
        Some(path) => Check::ok(s, "xcode-command-line-tools", first_line(&path)),
        None => Check::problem(
            s,
            "xcode-command-line-tools",
            CheckStatus::Fail,
            "xcode_clt_missing",
            "not installed; rustc links with the Apple linker they provide",
            "xcode-select --install",
        ),
    };
    let mut out = vec![
        xcode,
        Check::info(
            s,
            "metal",
            "ok",
            "part of macOS; Regia Engine renders through wgpu's Metal backend",
        ),
        check_spirv_val(s, probe),
    ];
    out.extend(check_editor_launcher(s, "macos", probe));
    out
}

/// The Android section: SDK, NDK, JDK, Gradle, adb, cargo-ndk and the Rust targets. Reads the real
/// environment through `android::toolchain`; the rustup and cargo-ndk parts go through `probe`.
fn check_android(probe: &dyn Probe) -> Vec<Check> {
    let s = "android";
    let mut out = Vec::new();
    let sdk = match toolchain::find_sdk() {
        Ok(sdk) => {
            out.push(Check::ok(s, "sdk", sdk.display().to_string()));
            Some(sdk)
        }
        Err(e) => {
            out.push(Check::problem(
                s,
                "sdk",
                CheckStatus::Fail,
                "android_sdk_missing",
                format!("{e:#}"),
                "install the Android SDK (Android Studio, or the command-line tools) and `export ANDROID_HOME=$HOME/Android/Sdk` (Windows: `setx ANDROID_HOME \"%LOCALAPPDATA%\\Android\\Sdk\"`)",
            ));
            None
        }
    };
    if let Some(sdk) = &sdk {
        out.extend(check_android_sdk_parts(probe, sdk));
        out.push(match toolchain::find_ndk(sdk) {
            Some(ndk) => {
                let version = toolchain::ndk_version(&ndk);
                let detail = match &version {
                    Some(v) => format!("{} (version {v})", ndk.display()),
                    None => ndk.display().to_string(),
                };
                match version.as_deref().and_then(ndk_major) {
                    Some(major) if major < MIN_NDK_MAJOR => Check::problem(
                        s,
                        "ndk",
                        CheckStatus::Fail,
                        "android_ndk_too_old",
                        format!("{detail}: r{MIN_NDK_MAJOR} or newer is required"),
                        "sdkmanager \"ndk;28.0.13004108\", or point ANDROID_NDK_HOME at an r27+ NDK",
                    ),
                    _ => Check::ok(s, "ndk", detail),
                }
            }
            None => Check::problem(
                s,
                "ndk",
                CheckStatus::Fail,
                "android_ndk_missing",
                "no NDK in the SDK and no ANDROID_NDK_HOME",
                "sdkmanager \"ndk;28.0.13004108\" (r27 or newer), or set ANDROID_NDK_HOME",
            ),
        });
    }
    out.push(match toolchain::check_java() {
        Ok(()) => Check::ok(
            s,
            "jdk",
            match regia_core::brand::env_var_os("JAVA_HOME") {
                Some(home) => format!("17+ at {}", PathBuf::from(home).display()),
                None => "17+ (from PATH)".to_string(),
            },
        ),
        Err(e) => Check::problem(
            s,
            "jdk",
            CheckStatus::Fail,
            "jdk_missing",
            format!("{e:#}"),
            "install a JDK 17 or newer and set JAVA_HOME",
        ),
    });
    out.push(match toolchain::find_gradle() {
        Ok(gradle) => Check::ok(s, "gradle", gradle.display().to_string()),
        Err(e) => Check::problem(
            s,
            "gradle",
            CheckStatus::Fail,
            "gradle_missing",
            format!("{e:#}"),
            "install Gradle 8 or newer, or let Android Studio download it once",
        ),
    });
    out.push(match toolchain::find_adb(sdk.as_deref()) {
        Ok(adb) => {
            let devices = process::try_output(Command::new(&adb).arg("devices"))
                .map_or(0, |list| list.lines().filter(|l| l.ends_with("device")).count());
            Check::ok(
                s,
                "adb",
                format!("{} ({devices} device(s) attached)", adb.display()),
            )
        }
        Err(e) => Check::problem(
            s,
            "adb",
            CheckStatus::Fail,
            "adb_missing",
            format!("{e:#}"),
            "install the SDK platform-tools (sdkmanager platform-tools)",
        ),
    });
    out.extend(check_android_rust(probe));
    out
}

/// cargo-ndk and the two Android Rust targets.
fn check_android_rust(probe: &dyn Probe) -> Vec<Check> {
    let s = "android";
    let mut out = vec![match probe.output("cargo", &["ndk", "--version"]) {
        Some(v) => Check::ok(s, "cargo-ndk", first_line(&v)),
        None => Check::problem(
            s,
            "cargo-ndk",
            CheckStatus::Fail,
            "cargo_ndk_missing",
            "not found; Android builds run through it",
            "cargo install cargo-ndk --locked",
        ),
    }];
    for triple in ["aarch64-linux-android", "x86_64-linux-android"] {
        out.push(rustup_target(s, probe, triple, "android_target_missing"));
    }
    out
}

/// One adapter as `regia doctor` prints it ([`ray_tracing_lines`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RtAdapterRow {
    index: usize,
    name: String,
    device_type: &'static str,
    backend: &'static str,
    ray_query: bool,
    vertex_return: bool,
    limits: AccelLimits,
    /// What a device on this adapter would get from a profile that asks for ray queries.
    if_requested: HwrtReason,
}

/// Probes every adapter (no device is created) and reports what hardware ray tracing each one
/// offers. Ray tracing is optional, so every check here is `info`.
fn check_ray_tracing() -> Vec<Check> {
    let rows: Vec<RtAdapterRow> = regia_render::hwrt::probe_adapters()
        .iter()
        .map(|a| RtAdapterRow {
            index: a.index,
            name: a.name.clone(),
            device_type: a.device_type_str(),
            backend: a.probe.backend.to_str(),
            ray_query: a.probe.ray_query,
            vertex_return: a.probe.vertex_return,
            limits: a.probe.limits,
            if_requested: a.decide(HwrtRequest::On).reason,
        })
        .collect();
    ray_tracing_lines(&rows)
        .into_iter()
        .map(|line| Check::info("gpu", "ray tracing", "ok", line))
        .collect()
}

/// The `[GPU ray tracing]` section for `rows`.
fn ray_tracing_lines(rows: &[RtAdapterRow]) -> Vec<String> {
    let mut lines = vec![
        "RenderProfile::sun_shadow_trace asks for ray-traced sun shadows, off in every preset (off = shadow map, on = ray queries or the screen trace, compute = compute fallback, screen = screen trace)"
            .to_string(),
    ];
    if rows.is_empty() {
        lines.push(
            "no adapter found: ray tracing cannot be probed (`regia doctor --platform linux` checks Vulkan)"
                .into(),
        );
        return lines;
    }
    for row in rows {
        lines.push(format!(
            "#{} {} ({}, {}): ray queries {}, vertex return {}",
            row.index,
            row.name,
            row.backend,
            row.device_type,
            if row.ray_query { "yes" } else { "no" },
            if row.vertex_return { "yes" } else { "no" },
        ));
        if row.ray_query {
            lines.push(format!(
                "    limits: {} BLAS primitives, {} BLAS geometries, {} TLAS instances, {} per stage",
                row.limits.max_blas_primitive_count,
                row.limits.max_blas_geometry_count,
                row.limits.max_tlas_instance_count,
                row.limits.max_acceleration_structures_per_shader_stage,
            ));
        }
        let outcome = if row.if_requested == HwrtReason::Enabled {
            "ray-traced sun shadows can use ray queries"
        } else {
            "the sun takes the screen-space trace"
        };
        lines.push(format!(
            "    with sun_shadow_trace on: {} ({outcome})",
            row.if_requested.code(),
        ));
    }
    if !rows.iter().any(|r| r.if_requested == HwrtReason::Enabled) {
        lines.push(
            "no adapter offers hardware ray queries (wgpu 30: Vulkan with VK_KHR_ray_query only); \
             this is optional"
                .into(),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: DoctorArgs,
    }

    /// A machine with exactly the tools in `tools` (`"program arg..."` → stdout).
    struct Fake {
        os: &'static str,
        tools: BTreeMap<String, String>,
        files: Vec<PathBuf>,
        dirs: Vec<PathBuf>,
        free: Option<u64>,
    }

    impl Fake {
        fn new(os: &'static str, tools: &[(&str, &str)]) -> Self {
            Self {
                os,
                tools: tools
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
                files: Vec::new(),
                dirs: Vec::new(),
                free: None,
            }
        }
    }

    impl Probe for Fake {
        fn output(&self, program: &str, args: &[&str]) -> Option<String> {
            let mut key = program.to_string();
            for a in args {
                key.push(' ');
                key.push_str(a);
            }
            self.tools.get(&key).cloned()
        }
        fn host_os(&self) -> &str {
            self.os
        }
        fn is_file(&self, path: &Path) -> bool {
            self.files.iter().any(|f| f == path)
        }
        fn non_empty_dir(&self, path: &Path) -> bool {
            self.dirs.iter().any(|d| d == path)
        }
        fn env(&self, _key: &str) -> Option<String> {
            None
        }
        fn free_bytes(&self, _path: &Path) -> Option<u64> {
            self.free
        }
    }

    fn codes(checks: &[Check]) -> Vec<&'static str> {
        checks.iter().map(|c| c.code).collect()
    }

    #[test]
    fn args_parse() {
        let args = Wrapper::try_parse_from(["x"]).expect("parse").args;
        assert_eq!(args.platform, None);
        assert_eq!(args.project, PathBuf::from("."));
        assert!(!args.json);

        let args = Wrapper::try_parse_from(["x", "--platform", "android", "--project", "/p", "--json"])
            .expect("parse")
            .args;
        assert_eq!(args.platform.as_deref(), Some("android"));
        assert_eq!(args.project, PathBuf::from("/p"));
        assert!(args.json);
    }

    #[test]
    fn an_unknown_platform_is_an_error_that_lists_the_valid_ones() {
        let err = sections_for(Some("ios"), "linux").expect_err("unknown platform");
        let text = err.to_string();
        assert!(text.contains("\"ios\""), "{text}");
        for p in PLATFORMS {
            assert!(text.contains(p), "{text}");
        }
    }

    #[test]
    fn platforms_select_their_sections() -> Result<()> {
        let names = |p: Option<&str>, os: &str| -> Result<Vec<&'static str>> {
            Ok(sections_for(p, os)?.into_iter().map(Section::name).collect())
        };
        assert_eq!(names(None, "linux")?, ["rust", "linux", "android", "gpu"]);
        assert_eq!(names(None, "windows")?, ["rust", "windows", "android", "gpu"]);
        assert_eq!(names(None, "macos")?, ["rust", "macos", "android", "gpu"]);
        assert_eq!(names(Some("windows"), "linux")?, ["rust", "windows", "gpu"]);
        assert_eq!(names(Some("deck"), "linux")?, ["rust", "deck"]);
        assert_eq!(names(Some("gpu"), "linux")?, ["gpu"]);
        // `all` used to run only the Rust section and the GPU probe.
        assert_eq!(
            names(Some("all"), "linux")?,
            ["rust", "linux", "windows", "deck", "macos", "android", "gpu"]
        );
        Ok(())
    }

    #[test]
    fn default_run_softens_android_failures_only() {
        let mk = |section: &'static str, status| Check {
            section,
            name: "x".into(),
            status,
            code: "x_missing",
            detail: String::new(),
            fix: None,
        };
        let mut checks = vec![
            mk("android", CheckStatus::Fail),
            mk("rust", CheckStatus::Fail),
            mk("android", CheckStatus::Ok),
        ];
        soften_android(&mut checks);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert_eq!(checks[1].status, CheckStatus::Fail);
        assert_eq!(checks[2].status, CheckStatus::Ok);
        assert!(finish(&checks).is_err(), "a real failure still fails");
        checks[1].status = CheckStatus::Ok;
        assert!(finish(&checks).is_ok());
    }

    #[test]
    fn missing_rust_tools_fail_and_rustup_only_warns() {
        let none = Fake::new("linux", &[]);
        let checks = check_rust(&none);
        assert_eq!(
            codes(&checks),
            ["rustc_missing", "cargo_missing", "rustup_missing"]
        );
        assert_eq!(checks[0].status, CheckStatus::Fail);
        assert_eq!(checks[2].status, CheckStatus::Warn);
        assert!(checks.iter().all(|c| c.fix.is_some()));
        let all = Fake::new(
            "linux",
            &[
                ("rustc --version", "rustc 1.99.0 (abc 2026-10-01)\n"),
                ("cargo --version", "cargo 1.99.0"),
                ("rustup --version", "rustup 1.29.0"),
            ],
        );
        let checks = check_rust(&all);
        assert!(checks.iter().all(|c| c.status == CheckStatus::Ok), "{checks:?}");
        assert_eq!(checks[0].detail, "rustc 1.99.0 (abc 2026-10-01)");
    }

    #[test]
    fn windows_cross_builds_need_cargo_xwin_the_msvc_target_and_clang_cl() {
        let bare = Fake::new(
            "linux",
            &[("rustup target list --installed", "x86_64-unknown-linux-gnu\n")],
        );
        let checks = check_windows(&bare);
        assert_eq!(
            codes(&checks),
            ["cargo_xwin_missing", "windows_target_missing", "clang_cl_missing"]
        );
        assert_eq!(
            checks[1].fix.as_deref(),
            Some("rustup target add x86_64-pc-windows-msvc")
        );
        assert_eq!(checks[2].status, CheckStatus::Warn);
        let ready = Fake::new(
            "linux",
            &[
                ("cargo xwin --version", "cargo-xwin 0.23.1"),
                (
                    "rustup target list --installed",
                    "x86_64-unknown-linux-gnu\nx86_64-pc-windows-msvc\n",
                ),
                ("clang-cl --version", "clang version 21"),
                ("lld-link --version", "LLD 21"),
            ],
        );
        assert!(check_windows(&ready).iter().all(|c| c.status == CheckStatus::Ok));
        // Without rustup the target cannot be checked: a failure that says so.
        let no_rustup = Fake::new("linux", &[("cargo xwin --version", "cargo-xwin 0.23.1")]);
        assert_eq!(check_windows(&no_rustup)[1].code, "rustup_missing");
    }

    #[test]
    fn on_windows_the_vulkan_loader_is_looked_up_and_its_absence_only_warns() {
        let mut host = Fake::new("windows", &[]);
        let checks = check_windows(&host);
        assert_eq!(codes(&checks), ["vulkan_loader_missing", "ok"]);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        host.files
            .push(Path::new(r"C:\Windows").join("System32").join("vulkan-1.dll"));
        assert_eq!(check_windows(&host)[0].status, CheckStatus::Ok);
    }

    #[test]
    fn the_deck_needs_linux_cargo_zigbuild_and_a_recent_zig() {
        let mac = Fake::new("macos", &[]);
        assert_eq!(codes(&check_deck(&mac)), ["deck_needs_linux"]);
        let old = Fake::new(
            "linux",
            &[
                ("cargo zigbuild --version", "cargo-zigbuild 0.20.1"),
                ("zig version", "0.13.0"),
            ],
        );
        assert_eq!(codes(&check_deck(&old)), ["ok", "zig_too_old"]);
        let ok = Fake::new(
            "linux",
            &[
                ("cargo zigbuild --version", "cargo-zigbuild 0.20.1"),
                ("zig version", "0.14.1"),
            ],
        );
        assert_eq!(codes(&check_deck(&ok)), ["ok", "ok"]);
        let dev = Fake::new(
            "linux",
            &[
                ("cargo zigbuild --version", "x"),
                ("zig version", "0.15.0-dev.1+abc"),
            ],
        );
        assert_eq!(codes(&check_deck(&dev)), ["ok", "ok"]);
        assert_eq!(
            codes(&check_deck(&Fake::new("linux", &[]))),
            ["cargo_zigbuild_missing", "zig_missing"]
        );
        assert_eq!(major_minor("0.14.1"), Some([0, 14]));
        assert_eq!(major_minor("garbage"), None);
    }

    #[test]
    fn macos_is_informational_off_the_mac_and_checks_the_toolchain_on_it() {
        let linux = check_macos(&Fake::new("linux", &[]));
        assert_eq!(codes(&linux), ["macos_testing_only"]);
        assert_eq!(linux[0].status, CheckStatus::Info);
        let mac = check_macos(&Fake::new("macos", &[]));
        assert_eq!(codes(&mac), ["xcode_clt_missing", "ok", "spirv_val_missing"]);
        assert_eq!(mac[2].fix.as_deref(), Some("brew install spirv-tools"));
        let ready = check_macos(&Fake::new(
            "macos",
            &[
                ("xcode-select -p", "/Library/Developer/CommandLineTools"),
                ("spirv-val --version", "SPIRV-Tools v2025.4"),
            ],
        ));
        assert!(
            ready
                .iter()
                .all(|c| c.status != CheckStatus::Fail && c.status != CheckStatus::Warn)
        );
    }

    #[test]
    fn linux_vulkan_lists_devices_and_tells_a_missing_tool_from_a_missing_driver() {
        let ok = Fake::new(
            "linux",
            &[(
                "vulkaninfo --summary",
                "GPU0:\n\tdeviceName = NVIDIA GeForce RTX 5080\nGPU1:\n\tdeviceName = llvmpipe\n",
            )],
        );
        let checks = check_linux(&ok);
        assert_eq!(checks[0].detail, "NVIDIA GeForce RTX 5080, llvmpipe");
        let mut driver_only = Fake::new("linux", &[]);
        driver_only.dirs.push(PathBuf::from("/usr/share/vulkan/icd.d"));
        assert_eq!(check_linux(&driver_only)[0].code, "vulkaninfo_missing");
        assert_eq!(check_linux(&Fake::new("linux", &[]))[0].code, "vulkan_missing");
        assert_eq!(check_linux(&Fake::new("linux", &[]))[1].code, "spirv_val_missing");
    }

    #[test]
    fn android_rust_checks_name_the_missing_targets() {
        let half = Fake::new(
            "linux",
            &[
                ("cargo ndk --version", "cargo-ndk 4.1.2"),
                ("rustup target list --installed", "aarch64-linux-android\n"),
            ],
        );
        let checks = check_android_rust(&half);
        assert_eq!(codes(&checks), ["ok", "ok", "android_target_missing"]);
        assert_eq!(
            checks[2].fix.as_deref(),
            Some("rustup target add x86_64-linux-android")
        );
    }

    #[test]
    fn every_code_is_lowercase_snake_case() {
        let fakes = [
            Fake::new("linux", &[]),
            Fake::new("macos", &[]),
            Fake::new("windows", &[]),
        ];
        for fake in &fakes {
            for section in [
                Section::Rust,
                Section::Linux,
                Section::Windows,
                Section::Deck,
                Section::Macos,
            ] {
                for c in checks_for(section, fake) {
                    assert!(
                        !c.code.is_empty()
                            && c.code
                                .chars()
                                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
                        "{c:?}"
                    );
                    assert_eq!(c.section, section.name());
                    // Every problem says how to fix it.
                    if matches!(c.status, CheckStatus::Warn | CheckStatus::Fail) {
                        assert!(c.fix.is_some(), "{c:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_json_report_lists_the_checks_and_the_failed_codes() {
        let checks = vec![
            Check::ok("rust", "rustc", "rustc 1.99.0"),
            Check::problem(
                "windows",
                "cargo-xwin",
                CheckStatus::Fail,
                "cargo_xwin_missing",
                "x",
                "y",
            ),
            Check::problem("rust", "rustup", CheckStatus::Warn, "rustup_missing", "x", "y"),
        ];
        let report = report_json(&checks, "linux", &[Section::Rust, Section::Windows]);
        assert_eq!(report["schema"], DOCTOR_SCHEMA);
        assert_eq!(report["kind"], "regia_doctor");
        assert_eq!(report["ok"], false);
        assert_eq!(report["failed"], json!(["cargo_xwin_missing"]));
        assert_eq!(report["warnings"], 1);
        assert_eq!(report["sections"], json!(["rust", "windows"]));
        assert_eq!(report["checks"][1]["status"], "fail");
        assert_eq!(report["checks"][0]["fix"], Value::Null);
        let text = report_text(&checks, &[Section::Rust, Section::Windows]);
        assert!(
            text.contains("[Windows]\n  FAIL cargo-xwin: x\n       fix: y"),
            "{text}"
        );
        assert!(text.contains("1 check(s) failed, 1 warning(s)"), "{text}");
        let err = finish(&checks).expect_err("a failed check is an error");
        assert_eq!(err.to_string(), "1 doctor check(s) failed: cargo_xwin_missing");
        assert!(finish(&checks[..1]).is_ok());
    }

    fn rt_row(index: usize, ray_query: bool, if_requested: HwrtReason) -> RtAdapterRow {
        RtAdapterRow {
            index,
            name: format!("gpu {index}"),
            device_type: "discrete",
            backend: "vulkan",
            ray_query,
            vertex_return: false,
            limits: AccelLimits {
                max_blas_primitive_count: 1 << 20,
                max_blas_geometry_count: 64,
                max_tlas_instance_count: 1 << 16,
                max_acceleration_structures_per_shader_stage: 8,
            },
            if_requested,
        }
    }

    #[test]
    fn gpu_platform_parses() {
        let args = Wrapper::try_parse_from(["x", "--platform", "gpu"])
            .expect("parse")
            .args;
        assert_eq!(args.platform.as_deref(), Some("gpu"));
    }

    #[test]
    fn ray_tracing_lines_name_each_adapter_and_its_outcome() {
        let rows = [
            rt_row(0, true, HwrtReason::Enabled),
            rt_row(1, false, HwrtReason::NoAdapterRayQuery),
        ];
        let lines = ray_tracing_lines(&rows);
        let text = lines.join("\n");
        assert!(lines[0].starts_with("RenderProfile::sun_shadow_trace"), "{text}");
        assert!(
            text.contains("#0 gpu 0 (vulkan, discrete): ray queries yes"),
            "{text}"
        );
        assert!(text.contains("1048576 BLAS primitives"), "{text}");
        assert!(
            text.contains("#1 gpu 1 (vulkan, discrete): ray queries no"),
            "{text}"
        );
        assert!(text.contains("with sun_shadow_trace on: hwrt_enabled"), "{text}");
        assert!(
            text.contains("with sun_shadow_trace on: hwrt_no_adapter_ray_query"),
            "{text}"
        );
        // Limits are printed only for adapters that offer ray queries.
        assert_eq!(
            lines.iter().filter(|l| l.contains("limits:")).count(),
            1,
            "{text}"
        );
        assert!(!text.contains("no adapter offers"), "{text}");
    }

    #[test]
    fn ray_tracing_lines_say_when_nothing_offers_ray_queries() {
        let rows = [rt_row(0, false, HwrtReason::NoAdapterRayQuery)];
        let lines = ray_tracing_lines(&rows);
        let text = lines.join("\n");
        assert!(text.contains("screen-space trace"), "{text}");
        assert!(text.contains("no adapter offers hardware ray queries"), "{text}");
        let none = ray_tracing_lines(&[]);
        assert_eq!(none.len(), 2);
        assert!(none[1].starts_with("no adapter found"));
    }

    #[test]
    fn df_output_is_parsed_to_free_bytes() {
        let df = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 1000 400 600 40% /\n";
        assert_eq!(parse_df_free_bytes(df), Some(600 * 1024));
        assert_eq!(parse_df_free_bytes("garbage"), None);
        assert_eq!(parse_df_free_bytes(""), None);
    }

    #[test]
    fn disk_space_is_a_failure_a_warning_or_fine() {
        let mut fake = Fake::new("linux", &[]);
        let dir = Path::new("/p");
        fake.free = Some(1 << 30);
        let c = check_disk(&fake, "rust", dir);
        assert_eq!((c.status, c.code), (CheckStatus::Fail, "disk_full"));
        assert!(c.fix.is_some());
        fake.free = Some(10 << 30);
        let c = check_disk(&fake, "rust", dir);
        assert_eq!((c.status, c.code), (CheckStatus::Warn, "disk_low"));
        assert!(c.fix.is_some());
        fake.free = Some(100 << 30);
        let c = check_disk(&fake, "rust", dir);
        assert_eq!((c.status, c.code), (CheckStatus::Ok, "ok"));
        assert!(c.detail.contains("100.0 GiB"), "{}", c.detail);
        fake.free = None;
        assert_eq!(check_disk(&fake, "rust", dir).status, CheckStatus::Info);
    }

    #[test]
    fn ndk_versions_are_compared_by_major() {
        assert_eq!(ndk_major("28.0.13004108"), Some(28));
        assert_eq!(ndk_major("26.1.10909125"), Some(26));
        assert_eq!(ndk_major("r27"), None);
        assert_eq!(ndk_major(""), None);
    }

    #[test]
    fn the_sdk_needs_a_platform_and_build_tools() {
        let sdk = Path::new("/sdk");
        let mut fake = Fake::new("linux", &[]);
        let checks = check_android_sdk_parts(&fake, sdk);
        assert_eq!(
            codes(&checks),
            ["android_platform_missing", "android_build_tools_missing"]
        );
        assert!(checks.iter().all(|c| c.status == CheckStatus::Fail));
        assert!(
            checks[0]
                .fix
                .as_deref()
                .is_some_and(|f| f.starts_with("sdkmanager"))
        );
        fake.dirs = vec![sdk.join("platforms"), sdk.join("build-tools")];
        assert_eq!(codes(&check_android_sdk_parts(&fake, sdk)), ["ok", "ok"]);
    }

    #[test]
    fn an_unknown_platform_is_a_usage_error_with_exit_code_two() {
        let err = sections_for(Some("ios"), "linux").expect_err("unknown platform");
        assert!(err.is::<crate::commands::assets_validate::UsageError>());
    }

    #[test]
    fn pak_verification_reports_json_and_text() {
        let dir = regia_test_utils::unique_temp_dir("doctor_paks");
        let missing = pak_rows(&dir);
        assert!(missing.is_err(), "an empty directory is an error");
        let rows = vec![
            PakRow {
                path: PathBuf::from("a.fpak"),
                status: "ok",
                detail: None,
            },
            PakRow {
                path: PathBuf::from("b.fpak"),
                status: "corrupt",
                detail: Some("expected 1, got 2".into()),
            },
        ];
        let report = pak_report_json(&rows);
        assert_eq!(report["kind"], "regia_doctor_paks");
        assert_eq!(report["ok"], false);
        assert_eq!(report["corrupt"], 1);
        assert_eq!(report["paks"][1]["status"], "corrupt");
        assert_eq!(report["paks"][0]["detail"], Value::Null);
        let text = pak_report_text(&rows);
        assert!(text.contains("[ok]   a.fpak"), "{text}");
        assert!(text.contains("[FAIL] b.fpak: expected 1, got 2"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wiring of `regia doctor` runs against an injected probe, not the real machine
    /// (sprint-deep-zai-cli-tools finding 6): the old smoke test spawned `rustc`, `cargo`,
    /// `rustup`, `vulkaninfo`, `df` and `adb devices` and created a Vulkan instance in every
    /// `cargo test` run, and its first half asserted nothing. On a bare linux machine with no
    /// tools the report keeps its documented shape (`book/agents/doctor.md`), Android failures
    /// are warnings, and the run fails naming the missing tools.
    #[test]
    fn a_bare_machine_report_fails_with_its_sections() {
        let args = DoctorArgs {
            platform: None,
            project: PathBuf::from("/p"),
            verify_paks: None,
            json: true,
        };
        let probe = Fake::new("linux", &[]);
        let gpu = || {
            vec![Check::info(
                "gpu",
                "ray tracing",
                "hwrt_unsupported",
                "stubbed: no adapter probe in tests",
            )]
        };
        let sections = sections_for(None, probe.host_os()).expect("sections");
        let checks = collect_checks(&args, &probe, gpu, &sections);
        let report = report_json(&checks, probe.host_os(), &sections);
        assert_eq!(report["schema"], DOCTOR_SCHEMA);
        assert_eq!(report["kind"], "regia_doctor");
        assert_eq!(report["host"], "linux");
        assert_eq!(report["sections"], json!(["rust", "linux", "android", "gpu"]));
        assert_eq!(report["ok"], false, "a machine without rustc fails");
        let failed = report["failed"].as_array().expect("failed codes");
        assert!(
            failed.iter().any(|c| c.as_str() == Some("rustc_missing")),
            "the missing tool is named: {failed:?}"
        );
        assert!(
            report["warnings"].as_u64().is_some_and(|w| w > 0),
            "the softened android failures are warnings"
        );
        assert!(
            checks
                .iter()
                .filter(|c| c.section == "android")
                .all(|c| c.status != CheckStatus::Fail),
            "without --platform android is a courtesy, not a gate"
        );
        assert!(finish(&checks).is_err(), "the run fails on the rust section");
        let err = run_with(&args, &probe, gpu).expect_err("failed checks");
        assert!(err.to_string().contains("rustc_missing"), "{err}");
    }

    #[test]
    fn doctor_runs_without_panicking() {
        // The cheap real-`run` arm: an unknown platform is refused before any probing.
        let args = DoctorArgs {
            platform: None,
            project: PathBuf::from("."),
            verify_paks: None,
            json: true,
        };
        let bad = DoctorArgs {
            platform: Some("nope".into()),
            ..args
        };
        assert!(run(&bad).is_err());
    }

    /// A host with a home directory and the given files (ADR-20000's launcher check).
    struct HomeProbe {
        os: &'static str,
        files: Vec<PathBuf>,
    }

    impl Probe for HomeProbe {
        fn output(&self, _program: &str, _args: &[&str]) -> Option<String> {
            None
        }
        fn host_os(&self) -> &str {
            self.os
        }
        fn is_file(&self, path: &Path) -> bool {
            self.files.iter().any(|f| f == path)
        }
        fn non_empty_dir(&self, _path: &Path) -> bool {
            false
        }
        fn env(&self, key: &str) -> Option<String> {
            match key {
                "HOME" => Some("/home/dev".to_owned()),
                "APPDATA" => Some("/AppData/Roaming".to_owned()),
                _ => None,
            }
        }
        fn free_bytes(&self, _path: &Path) -> Option<u64> {
            None
        }
    }

    #[test]
    fn a_missing_editor_launcher_is_info_and_an_installed_one_ok() {
        let desktop = PathBuf::from("/home/dev/.local/share/applications/regia.editor.desktop");
        let mut probe = HomeProbe {
            os: "linux",
            files: Vec::new(),
        };
        let check = check_editor_launcher("linux", "linux", &probe).expect("checked");
        assert_eq!(
            (check.status, check.code),
            (CheckStatus::Info, "editor_launcher_missing")
        );
        assert!(
            check.detail.contains("regia editor install-launcher"),
            "{}",
            check.detail
        );
        probe.files.push(desktop);
        let check = check_editor_launcher("linux", "linux", &probe).expect("checked");
        assert_eq!((check.status, check.code), (CheckStatus::Ok, "ok"));
        // Only on the host's own section.
        assert!(check_editor_launcher("windows", "windows", &probe).is_none());
        let mac = HomeProbe {
            os: "macos",
            files: vec![PathBuf::from(
                "/home/dev/Applications/Regia Editor.app/Contents/Info.plist",
            )],
        };
        assert_eq!(
            check_editor_launcher("macos", "macos", &mac)
                .expect("checked")
                .code,
            "ok"
        );
        let windows = HomeProbe {
            os: "windows",
            files: Vec::new(),
        };
        let check = check_editor_launcher("windows", "windows", &windows).expect("checked");
        assert!(check.detail.contains("Regia Editor.lnk"), "{}", check.detail);
        // No home directory (the other tests' probe): no check at all.
        assert!(check_editor_launcher("linux", "linux", &Fake::new("linux", &[])).is_none());
    }
}
