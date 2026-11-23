// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask dist`: test bundles of engine binaries for Windows and the Steam Deck (ADR-868,
//! book `user-guide/test-bundles.md`).
//!
//! A bundle is what the owner copies to a Windows PC or a Deck to run a pass there without a
//! Rust toolchain: the binaries (default `vfx_lab`), the engine license and third-party notices,
//! a `README.txt` that says how to run it and where the program writes its logs, crash dumps and
//! pipeline cache, optional pre-built pipeline caches, and `dist-manifest.json`, which lists every
//! file with its size and BLAKE3 hash, the binary checks and the runtime directories.
//!
//! ```text
//! target/dist/<target>/
//!   regia-<target>-<version>/          the bundle
//!     vfx_lab(.exe)
//!     run-<bin>.cmd                      Windows launcher (not with --no-launcher)
//!     *.dll                              only with --dlls (Windows)
//!     dxcompiler.dll, dxil.dll           only with --dxc (Windows, Direct3D 12 DXC)
//!     licenses/LICENSE (MPL-2.0), licenses/THIRD_PARTY_NOTICES.md
//!     content/pipelines/*.fpc            only with --pipeline-caches
//!     README.txt
//!     dist-manifest.json
//!   regia-<target>-<version>.zip|.tar  the same tree, one top-level folder
//!   symbols/<bin>.pdb                    Windows, when the linker wrote one (not in the archive)
//! ```
//!
//! - Windows: `cargo xwin build --target x86_64-pc-windows-msvc` on Linux (`cargo build` on
//!   Windows), a stored zip. The exe's DLL imports are read from its PE header; a DLL that is not
//!   part of Windows, not the Visual C++ runtime, not the GPU driver's `vulkan-1.dll` and not in
//!   the bundle fails the bundle.
//! - Deck: `cargo zigbuild --target x86_64-unknown-linux-gnu.2.31` (the glibc of Steam Linux
//!   Runtime 3.0), a ustar tar that keeps the executable bit. A binary that needs a newer glibc
//!   fails the bundle; libraries the runtime lacks are reported.
//!
//! `--dry-run` prints the plan (build command, every file and where it comes from, the runtime
//! directories) as JSON and touches nothing, so both targets can be planned on any Linux machine.
//! `--dry-run --manifest-out FILE` also writes the manifest the bundle would get ([`preview_manifest`]:
//! the same keys, `"dry_run": true`, the files not built yet under `missing`); `--verify` refuses it.
//! `--from DIR` skips the build and bundles binaries built elsewhere. `--verify PATH` checks a
//! bundle folder or its archive against the manifest inside it.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::dist_archive::{self, Entry};
use crate::dist_binary::{self, DllClass};

/// File name of the manifest at the root of every bundle.
pub const MANIFEST_FILE: &str = "dist-manifest.json";

/// Version of `dist-manifest.json`; [`verify`] refuses any other.
pub const SCHEMA: u64 = 1;

/// The `kind` field of the manifest.
pub const KIND: &str = "regia_dist";

/// Binaries bundled when no `--bin` is given: the measured sample the Windows pass and the Deck
/// test run use.
pub const DEFAULT_BINS: &[&str] = &["vfx_lab"];

/// Platform of a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Target {
    /// Windows 10/11 x64 (MSVC).
    Windows,
    /// Steam Deck: x86-64 Linux against the Steam Linux Runtime 3.0 glibc (2.31).
    Deck,
}

/// One directory the bundled program writes to at run time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeDir {
    /// Manifest key (`user`, `logs`, `crash_dumps`, `cache`, `pipeline_cache`,
    /// `shipped_pipeline_caches`).
    pub key: &'static str,
    /// The path, with `<exe>` for the executable's file stem.
    pub path: &'static str,
    /// What lives there.
    pub what: &'static str,
}

/// Windows run-time directories (`regia_platform::paths`; crash dumps per ADR-525).
const WINDOWS_DIRS: &[RuntimeDir] = &[
    RuntimeDir {
        key: "user",
        path: r"%APPDATA%\RegiaEngine\<exe>",
        what: "settings and saves (user://)",
    },
    RuntimeDir {
        key: "logs",
        path: r"%APPDATA%\RegiaEngine\<exe>\logs",
        what: "the session log and panic reports (logs://)",
    },
    RuntimeDir {
        key: "crash_dumps",
        path: r"%LOCALAPPDATA%\RegiaEngine\<exe>\feedback",
        what: "feedback and crash bundles and native minidumps; native capture needs REGIA_NATIVE_CRASH=1 on Windows",
    },
    RuntimeDir {
        key: "cache",
        path: r"%LOCALAPPDATA%\RegiaEngine\<exe>",
        what: "cache:// (safe to delete)",
    },
    RuntimeDir {
        key: "pipeline_cache",
        path: r"%LOCALAPPDATA%\RegiaEngine\<exe>\pipelines",
        what: "the Vulkan pipeline cache (Direct3D 12 has none in wgpu 30); REGIA_PIPELINE_CACHE_DIR overrides it",
    },
    RuntimeDir {
        key: "shipped_pipeline_caches",
        path: r"<bundle>\content\pipelines",
        what: "pre-built pipeline caches that seed a matching GPU and driver on the first launch",
    },
];

/// Steam Deck (Linux) run-time directories (`regia_platform::paths`, XDG base directories; the
/// Steam Linux Runtime keeps the real home directory).
const DECK_DIRS: &[RuntimeDir] = &[
    RuntimeDir {
        key: "user",
        path: "~/.local/share/RegiaEngine/<exe>",
        what: "settings and saves (user://)",
    },
    RuntimeDir {
        key: "logs",
        path: "~/.local/share/RegiaEngine/<exe>/logs",
        what: "the session log and panic reports (logs://)",
    },
    RuntimeDir {
        key: "crash_dumps",
        path: "~/.local/share/RegiaEngine/<exe>/feedback",
        what: "feedback and crash bundles and native minidumps",
    },
    RuntimeDir {
        key: "cache",
        path: "~/.cache/RegiaEngine/<exe>",
        what: "cache:// (safe to delete)",
    },
    RuntimeDir {
        key: "pipeline_cache",
        path: "~/.cache/RegiaEngine/<exe>/pipelines",
        what: "the Vulkan pipeline cache; REGIA_PIPELINE_CACHE_DIR overrides it",
    },
    RuntimeDir {
        key: "shipped_pipeline_caches",
        path: "<bundle>/content/pipelines",
        what: "pre-built pipeline caches that seed a matching GPU and driver on the first launch",
    },
];

impl Target {
    /// `windows` or `deck`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Deck => "deck",
        }
    }

    /// Parses [`Target::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "windows" => Some(Self::Windows),
            "deck" => Some(Self::Deck),
            _ => None,
        }
    }

    /// The Rust target triple (the directory cargo writes to).
    #[must_use]
    pub fn triple(self) -> &'static str {
        match self {
            Self::Windows => "x86_64-pc-windows-msvc",
            Self::Deck => "x86_64-unknown-linux-gnu",
        }
    }

    /// The file name of binary `bin`.
    #[must_use]
    pub fn binary_file(self, bin: &str) -> String {
        match self {
            Self::Windows => format!("{bin}.exe"),
            Self::Deck => bin.to_string(),
        }
    }

    /// The archive extension: `zip` or `tar`.
    #[must_use]
    pub fn archive_ext(self) -> &'static str {
        match self {
            Self::Windows => "zip",
            Self::Deck => "tar",
        }
    }

    /// The directories the bundled program writes to.
    #[must_use]
    pub fn runtime_dirs(self) -> &'static [RuntimeDir] {
        match self {
            Self::Windows => WINDOWS_DIRS,
            Self::Deck => DECK_DIRS,
        }
    }

    /// The glibc floor (Deck only).
    #[must_use]
    pub fn glibc_floor(self) -> Option<&'static [u32]> {
        match self {
            Self::Windows => None,
            Self::Deck => Some(&dist_binary::STEAM_RUNTIME_GLIBC),
        }
    }
}

/// Command-line options of `cargo xtask dist`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// Platform of the bundle: `windows` or `deck`. Required unless `--verify`.
    #[arg(long, value_enum)]
    pub target: Option<Target>,
    /// A binary to bundle, `<package>` or `<package>:<bin>` (repeatable; default `vfx_lab`).
    #[arg(long = "bin", value_name = "PACKAGE[:BIN]")]
    pub bins: Vec<String>,
    /// Cargo profile (`release`, `shipping`, `dev` or a custom one).
    #[arg(long, default_value = "release")]
    pub profile: String,
    /// Output directory (default `target/dist/<target>`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Bundle binaries already built in this directory instead of building them.
    #[arg(long, value_name = "DIR")]
    pub from: Option<PathBuf>,
    /// Ship the `.fpc` pipeline caches of this directory in `content/pipelines/` (each is
    /// validated and stored under its key's shipped name).
    #[arg(long, value_name = "DIR")]
    pub pipeline_caches: Option<PathBuf>,
    /// Windows only: ship every `*.dll` of this directory next to the exe (the DLL set the exe
    /// imports that Windows and the GPU driver do not provide, e.g. `steam_api64.dll`).
    #[arg(long, value_name = "DIR")]
    pub dlls: Option<PathBuf>,
    /// Windows only: ship `dxcompiler.dll` and `dxil.dll` from this directory next to the exe, so
    /// Direct3D 12 compiles shaders with DXC (Shader Model 6) at run time. Both files are
    /// required. Without it the bundle relies on the exe's own DXC (`static-dxc`) or on FXC.
    #[arg(long, value_name = "DIR")]
    pub dxc: Option<PathBuf>,
    /// Windows only: do not write the `run-<bin>.cmd` launcher scripts.
    #[arg(long)]
    pub no_launcher: bool,
    /// Windows only: link the C runtime statically (`-Ctarget-feature=+crt-static`), so the exe
    /// does not need the Visual C++ Redistributable. Not yet tried with every C dependency.
    #[arg(long)]
    pub crt_static: bool,
    /// Ship every file under this directory in the bundle, at the same relative path (`demo-package`
    /// puts the cooked scene and the launchers here). A file whose path a plan file already holds
    /// fails the plan; `*.sh` files and files with an executable bit keep it.
    #[arg(long, value_name = "DIR")]
    pub overlay: Option<PathBuf>,
    /// Write the bundle folder only, no zip or tar.
    #[arg(long)]
    pub no_archive: bool,
    /// Print the plan as JSON and stop: no build, nothing written.
    #[arg(long)]
    pub dry_run: bool,
    /// With `--dry-run`: also write the manifest the bundle would get to this file (ADR-1379).
    /// Files that exist already (licenses, the README) carry their size and BLAKE3; binaries that
    /// are not built yet carry `null` and are listed under `missing`.
    #[arg(long, value_name = "FILE", requires = "dry_run")]
    pub manifest_out: Option<PathBuf>,
    /// Check a bundle folder, or its `.zip` / `.tar`, against its manifest and exit.
    #[arg(long, value_name = "PATH")]
    pub verify: Option<PathBuf>,
    /// The source revision the binaries were built from, recorded as the manifest's `git` and in
    /// the README (default: `REGIA_GIT_HASH`, then the repository's HEAD). Give it with `--from`
    /// when the binaries were built from another commit (ADR-5002).
    #[arg(long, value_name = "REV")]
    pub git: Option<String>,
    /// Ship this file as `README.txt` instead of the generated tester README (`demo-package`
    /// passes the player README here, ADR-5002).
    #[arg(long, value_name = "FILE")]
    pub readme: Option<PathBuf>,
    /// Ship this file as `licenses/THIRD_PARTY_NOTICES.md` instead of the workspace's engine-wide notices
    /// (`demo-package` passes the demo's own list, `docs/demo/THIRD_PARTY_NOTICES-demo.md`, ADR-5419).
    #[arg(long, value_name = "FILE")]
    pub notices: Option<PathBuf>,
    /// Build without the packages' default features (`demo-package` builds `showcase --no-default-features
    /// --features demo`, the agent protocol compiled out, ADR-5419).
    #[arg(long)]
    pub no_default_features: bool,
    /// Cargo features to build with, comma separated.
    #[arg(long, value_name = "FEATURES")]
    pub features: Option<String>,
}

/// One binary to bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinSpec {
    /// Cargo package.
    pub package: String,
    /// Binary target in that package.
    pub bin: String,
}

impl BinSpec {
    /// Parses `<package>` (binary of the same name) or `<package>:<bin>`.
    ///
    /// # Errors
    /// An empty part or a character outside `[A-Za-z0-9_-]`.
    pub fn parse(text: &str) -> Result<Self> {
        let (package, bin) = text.split_once(':').unwrap_or((text, text));
        for part in [package, bin] {
            if part.is_empty()
                || !part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                bail!("--bin {text:?}: expected <package> or <package>:<bin> (letters, digits, `_`, `-`)");
            }
        }
        Ok(Self {
            package: package.to_string(),
            bin: bin.to_string(),
        })
    }
}

/// The cargo invocation that builds a bundle's binaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildCommand {
    /// `cargo-xwin`, `cargo-zigbuild` or `cargo` (for the manifest).
    pub tool: &'static str,
    /// Program to run (`cargo`).
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
}

impl BuildCommand {
    /// The command as one line.
    #[must_use]
    pub fn display(&self) -> String {
        let mut line = self.program.clone();
        for arg in &self.args {
            line.push(' ');
            if arg.contains([' ', '"', '[']) {
                line.push('\'');
                line.push_str(arg);
                line.push('\'');
            } else {
                line.push_str(arg);
            }
        }
        line
    }
}

/// The directory cargo writes `profile`'s artifacts to (`dev` → `debug`).
#[must_use]
pub fn profile_dir(profile: &str) -> &str {
    match profile {
        "dev" | "test" => "debug",
        "bench" => "release",
        other => other,
    }
}

/// The build command for `target` on a `host_os` (`std::env::consts::OS`) machine.
///
/// # Errors
/// A Deck bundle off Linux (cargo-zigbuild targets glibc from Linux only), or `--crt-static` for
/// the Deck.
pub fn build_command(
    target: Target,
    host_os: &str,
    profile: &str,
    bins: &[BinSpec],
    crt_static: bool,
) -> Result<BuildCommand> {
    let (tool, mut args, triple) = match target {
        Target::Windows if host_os == "windows" => {
            ("cargo", vec!["build".to_string()], target.triple().to_string())
        }
        Target::Windows => (
            "cargo-xwin",
            vec!["xwin".to_string(), "build".to_string()],
            target.triple().to_string(),
        ),
        Target::Deck if host_os == "linux" => {
            let floor = dist_binary::version_text(&dist_binary::STEAM_RUNTIME_GLIBC);
            (
                "cargo-zigbuild",
                vec!["zigbuild".to_string()],
                format!("{}.{floor}", target.triple()),
            )
        }
        Target::Deck => bail!(
            "the Steam Deck bundle builds on Linux (cargo-zigbuild against glibc 2.31); on {host_os}, \
             build there and pass --from <dir>"
        ),
    };
    if crt_static && target != Target::Windows {
        bail!("--crt-static applies to Windows bundles only");
    }
    args.extend([
        "--profile".to_string(),
        profile.to_string(),
        "--target".to_string(),
        triple,
    ]);
    let mut packages: Vec<&str> = Vec::new();
    for spec in bins {
        if !packages.contains(&spec.package.as_str()) {
            packages.push(&spec.package);
        }
    }
    for package in packages {
        args.extend(["-p".to_string(), package.to_string()]);
    }
    for spec in bins {
        args.extend(["--bin".to_string(), spec.bin.clone()]);
    }
    if crt_static {
        args.extend([
            "--config".to_string(),
            format!(
                "target.{}.rustflags=[\"-Ctarget-feature=+crt-static\"]",
                target.triple()
            ),
        ]);
    }
    Ok(BuildCommand {
        tool,
        program: "cargo".to_string(),
        args,
    })
}

/// What a bundle file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A program.
    Binary,
    /// The engine license or the third-party notices.
    License,
    /// The generated `README.txt`.
    Readme,
    /// A shipped pipeline cache.
    PipelineCache,
    /// A DLL shipped next to the exe (`--dlls`).
    Dll,
    /// `dxcompiler.dll` or `dxil.dll` (`--dxc`).
    Dxc,
    /// A generated launcher script (`run-<bin>.cmd`).
    Launcher,
    /// A file of the `--overlay` directory.
    Overlay,
}

impl Role {
    /// The manifest label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::License => "license",
            Self::Readme => "readme",
            Self::PipelineCache => "pipeline_cache",
            Self::Dll => "dll",
            Self::Dxc => "dxc",
            Self::Launcher => "launcher",
            Self::Overlay => "overlay",
        }
    }
}

/// One file of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFile {
    /// `/`-separated path inside the bundle (or inside `symbols/` for [`Plan::symbols`]).
    pub dest: String,
    /// Where it is copied from; `None` for generated files.
    pub source: Option<PathBuf>,
    /// What it is.
    pub role: Role,
    /// Gets the executable bit (`0o755`).
    pub executable: bool,
}

/// Everything `dist` will do, computed before anything is built or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Target platform.
    pub target: Target,
    /// Bundle folder name, `regia-<target>-<version>`.
    pub bundle: String,
    /// Cargo profile.
    pub profile: String,
    /// Binaries.
    pub bins: Vec<BinSpec>,
    /// The build, or `None` with `--from`.
    pub build: Option<BuildCommand>,
    /// Where the binaries are read from.
    pub binary_dir: PathBuf,
    /// Output directory: the bundle folder, the archive and `symbols/` go here.
    pub out: PathBuf,
    /// Files of the bundle (the manifest is added last and is not listed here).
    pub files: Vec<PlanFile>,
    /// Debug symbols kept next to the bundle when the build wrote them (Windows `.pdb`).
    pub symbols: Vec<PlanFile>,
    /// The archive, or `None` with `--no-archive`.
    pub archive: Option<PathBuf>,
}

/// The engine version the bundle is named after (the workspace version).
#[must_use]
pub fn engine_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Plans a bundle. Pure apart from the `--pipeline-caches` directory listing: nothing is built
/// or written. `target_dir` is cargo's target directory; `host_os` is `std::env::consts::OS`.
///
/// # Errors
/// No `--target`, a bad `--bin`, a target this host cannot build, or an unreadable
/// `--pipeline-caches` directory.
pub fn plan(args: &Args, root: &Path, target_dir: &Path, host_os: &str) -> Result<Plan> {
    let target = args.target.context("--target windows|deck is required")?;
    let bins = if args.bins.is_empty() {
        DEFAULT_BINS
            .iter()
            .map(|b| BinSpec::parse(b))
            .collect::<Result<Vec<_>>>()?
    } else {
        args.bins
            .iter()
            .map(|b| BinSpec::parse(b))
            .collect::<Result<Vec<_>>>()?
    };
    let mut seen = Vec::new();
    for spec in &bins {
        if seen.contains(&&spec.bin) {
            bail!("--bin {} is listed twice", spec.bin);
        }
        seen.push(&spec.bin);
    }
    let (build, binary_dir) = match &args.from {
        Some(dir) => {
            if args.crt_static {
                bail!("--crt-static changes the build; it cannot apply to --from binaries");
            }
            (None, dir.clone())
        }
        None => {
            let mut build = build_command(target, host_os, &args.profile, &bins, args.crt_static)?;
            if args.no_default_features {
                build.args.push("--no-default-features".to_string());
            }
            if let Some(features) = &args.features {
                build.args.extend(["--features".to_string(), features.clone()]);
            }
            (
                Some(build),
                target_dir.join(target.triple()).join(profile_dir(&args.profile)),
            )
        }
    };
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| target_dir.join("dist").join(target.name()));
    let bundle = format!("regia-{}-{}", target.name(), engine_version());

    let mut files = Vec::new();
    let mut symbols = Vec::new();
    for spec in &bins {
        let file = target.binary_file(&spec.bin);
        files.push(PlanFile {
            dest: file.clone(),
            source: Some(binary_dir.join(&file)),
            role: Role::Binary,
            executable: true,
        });
        if target == Target::Windows {
            // The MSVC linker names the PDB after the crate (`-` becomes `_`).
            let pdb = format!("{}.pdb", spec.bin.replace('-', "_"));
            symbols.push(PlanFile {
                dest: pdb.clone(),
                source: Some(binary_dir.join(&pdb)),
                role: Role::Binary,
                executable: false,
            });
        }
    }
    if target != Target::Windows && (args.dlls.is_some() || args.dxc.is_some() || args.no_launcher) {
        bail!("--dlls, --dxc and --no-launcher apply to --target windows only");
    }
    if let Some(dir) = &args.dxc {
        for name in DXC_FILES {
            let source = dir.join(name);
            if !source.is_file() {
                bail!(
                    "--dxc {}: {name} is missing (Direct3D 12 needs both {} next to the exe)",
                    dir.display(),
                    DXC_FILES.join(" and ")
                );
            }
            files.push(PlanFile {
                dest: (*name).to_string(),
                source: Some(source),
                role: Role::Dxc,
                executable: false,
            });
        }
    }
    if let Some(dir) = &args.dlls {
        for (name, source) in dll_set(dir)? {
            if files.iter().any(|f| f.dest.eq_ignore_ascii_case(&name)) {
                bail!(
                    "--dlls {}: {name} is also given by --dxc or collides with a bundle file",
                    dir.display()
                );
            }
            files.push(PlanFile {
                dest: name,
                source: Some(source),
                role: Role::Dll,
                executable: false,
            });
        }
    }
    if target == Target::Windows && !args.no_launcher {
        for spec in &bins {
            files.push(PlanFile {
                dest: launcher_name(&spec.bin),
                source: None,
                role: Role::Launcher,
                executable: false,
            });
        }
    }
    for name in ["LICENSE", "THIRD_PARTY_NOTICES.md"] {
        let source = match (&args.notices, name) {
            (Some(notices), "THIRD_PARTY_NOTICES.md") => notices.clone(),
            _ => root.join(name),
        };
        files.push(PlanFile {
            dest: format!("licenses/{name}"),
            source: Some(source),
            role: Role::License,
            executable: false,
        });
    }
    if let Some(dir) = &args.pipeline_caches {
        for (name, source) in pipeline_caches(dir)? {
            files.push(PlanFile {
                dest: format!("content/pipelines/{name}"),
                source: Some(source),
                role: Role::PipelineCache,
                executable: false,
            });
        }
    }
    files.push(PlanFile {
        dest: "README.txt".to_string(),
        source: args.readme.clone(),
        role: Role::Readme,
        executable: false,
    });
    if let Some(dir) = &args.overlay {
        for (rel, source, executable) in overlay_files(dir)? {
            if files.iter().any(|f| f.dest == rel) {
                bail!("--overlay {}: {rel} is already a bundle file", dir.display());
            }
            files.push(PlanFile {
                dest: rel,
                source: Some(source),
                role: Role::Overlay,
                executable,
            });
        }
    }
    let archive = (!args.no_archive).then(|| out.join(format!("{bundle}.{}", target.archive_ext())));
    Ok(Plan {
        target,
        bundle,
        profile: args.profile.clone(),
        bins,
        build,
        binary_dir,
        out,
        files,
        symbols,
        archive,
    })
}

/// The files under an `--overlay` directory as (`/`-separated relative path, source, executable),
/// sorted by path. A name that is not plain (`..`, empty or a non-UTF-8 part) is refused.
///
/// # Errors
/// An unreadable directory or a path that cannot be a bundle path.
pub fn overlay_files(dir: &Path) -> Result<Vec<(String, PathBuf, bool)>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf, bool)>) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("--overlay {}", dir.display()))? {
            let entry = entry.with_context(|| format!("--overlay {}", dir.display()))?;
            let path = entry.path();
            let kind = entry.file_type().with_context(|| format!("{}", path.display()))?;
            if kind.is_dir() {
                walk(base, &path, out)?;
            } else if kind.is_file() {
                let rel = path.strip_prefix(base).unwrap_or(&path);
                let mut parts = Vec::new();
                for part in rel.components() {
                    let std::path::Component::Normal(part) = part else {
                        bail!("--overlay: {} is not a plain relative path", rel.display());
                    };
                    let part = part
                        .to_str()
                        .with_context(|| format!("--overlay: {} is not UTF-8", rel.display()))?;
                    parts.push(part.to_string());
                }
                let executable = path.extension().is_some_and(|e| e == "sh") || is_executable(&path);
                out.push((parts.join("/"), path, executable));
            } else {
                bail!("--overlay: {} is a link or special file", path.display());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// True when `path` has an executable bit (always false off Unix).
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// The DXC runtime files Direct3D 12 needs next to the exe for Shader Model 6 (`dxil.dll` signs
/// the shaders `dxcompiler.dll` compiles; one without the other fails at run time).
pub const DXC_FILES: &[&str] = &["dxcompiler.dll", "dxil.dll"];

/// File name of the launcher script of `bin`.
#[must_use]
pub fn launcher_name(bin: &str) -> String {
    format!("run-{bin}.cmd")
}

/// The text of the launcher script of `bin` (CRLF line ends, as `cmd.exe` expects): runs from the
/// bundle folder wherever it was unzipped, turns native crash capture on, passes the arguments
/// through, and keeps the window open when the program fails so the message can be read.
#[must_use]
pub fn launcher_script(bin: &str) -> String {
    let lines = [
        "@echo off".to_string(),
        format!("rem Regia Engine launcher for {bin}.exe (written by cargo xtask dist)."),
        "setlocal".to_string(),
        "cd /d \"%~dp0\"".to_string(),
        "if not defined REGIA_NATIVE_CRASH set REGIA_NATIVE_CRASH=1".to_string(),
        format!("\"%~dp0{bin}.exe\" %*"),
        "set CODE=%ERRORLEVEL%".to_string(),
        "if not \"%CODE%\"==\"0\" (".to_string(),
        format!("  echo {bin}.exe exited with code %CODE%. Logs and crash dumps: see README.txt."),
        "  pause".to_string(),
        ")".to_string(),
        "exit /b %CODE%".to_string(),
    ];
    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    text
}

/// The `*.dll` files of `dir` (sorted by lower-case name) mapped to their paths.
///
/// # Errors
/// An unreadable directory, or two files that differ only by case.
pub fn dll_set(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut out: BTreeMap<String, (String, PathBuf)> = BTreeMap::new();
    for entry in fs::read_dir(dir).with_context(|| format!("--dlls {}", dir.display()))? {
        let path = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if !path.is_file() || !name.to_ascii_lowercase().ends_with(".dll") {
            continue;
        }
        if out
            .insert(name.to_ascii_lowercase(), (name.clone(), path))
            .is_some()
        {
            bail!(
                "--dlls {}: {name} appears twice (names differ only by case)",
                dir.display()
            );
        }
    }
    Ok(out.into_values().collect())
}

/// Validates every `.fpc` file of `dir` (header, key, checksum) and maps its shipped file name to
/// its path; two files with one key keep the larger. A corrupt cache is an error, never shipped.
///
/// # Errors
/// An unreadable directory or file, or an invalid cache.
pub fn pipeline_caches(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    use regia_render::pipeline_cache;
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("--pipeline-caches {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "fpc"))
        .collect();
    files.sort();
    let mut picked: BTreeMap<String, (PathBuf, usize)> = BTreeMap::new();
    for path in files {
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let key = pipeline_cache::read_key(&bytes)
            .map_err(|reason| anyhow::anyhow!("pipeline cache {}: {reason}", path.display()))?;
        pipeline_cache::decode(&bytes, &key)
            .map_err(|reason| anyhow::anyhow!("pipeline cache {}: {reason}", path.display()))?;
        let name = key.shipped_file_name();
        if picked.get(&name).is_none_or(|(_, size)| bytes.len() > *size) {
            picked.insert(name, (path, bytes.len()));
        }
    }
    Ok(picked.into_iter().map(|(name, (path, _))| (name, path)).collect())
}

/// The plan as the JSON `--dry-run` prints.
#[must_use]
pub fn plan_json(plan: &Plan) -> Value {
    let file = |f: &PlanFile| {
        json!({
            "path": f.dest,
            "role": f.role.label(),
            "source": f.source.as_ref().map(|p| p.display().to_string()),
            "mode": if f.executable { "755" } else { "644" },
        })
    };
    json!({
        "target": plan.target.name(),
        "triple": plan.target.triple(),
        "bundle": plan.bundle,
        "profile": plan.profile,
        "glibc_floor": plan.target.glibc_floor().map(dist_binary::version_text),
        "build": plan.build.as_ref().map(|b| json!({"tool": b.tool, "command": b.display()})),
        "binary_dir": plan.binary_dir.display().to_string(),
        "out": plan.out.display().to_string(),
        "bundle_dir": plan.out.join(&plan.bundle).display().to_string(),
        "archive": plan.archive.as_ref().map(|p| p.display().to_string()),
        "files": plan.files.iter().map(file).collect::<Vec<_>>(),
        "symbols": plan.symbols.iter().map(file).collect::<Vec<_>>(),
        "dx12": dx12_json(plan),
        "runtime_dirs": runtime_dirs_json(plan.target),
    })
}

/// The manifest a staged bundle of `plan` would get, computed without building or writing
/// anything (`--dry-run --manifest-out`, ADR-1379). It has every key of the staged manifest, plus
/// `"dry_run": true` and `missing` (the bundle paths whose source does not exist yet, usually the
/// binaries of a dry run on Linux). A file whose source exists carries its real size and BLAKE3;
/// the README is hashed as `stage` would write it before binary checks add notes; a missing
/// file carries `null`. `binaries` stays empty: the PE/ELF checks need the built files.
///
/// # Errors
/// A source that exists but cannot be read.
pub fn preview_manifest(plan: &Plan, git: &str) -> Result<Value> {
    let readme_text = readme(plan, git, &[]);
    let sorted: BTreeMap<&str, &PlanFile> = plan.files.iter().map(|f| (f.dest.as_str(), f)).collect();
    let mut files = Vec::new();
    let mut missing = Vec::new();
    for f in sorted.into_values() {
        let (blake3, bytes) = match &f.source {
            Some(source) if source.is_file() => {
                let (hash, size) = hash_file(source)?;
                (Value::from(hash), Value::from(size))
            }
            Some(_) => {
                missing.push(f.dest.clone());
                (Value::Null, Value::Null)
            }
            None => {
                let text = generated_text(f, &readme_text);
                (
                    Value::from(blake3::hash(text.as_bytes()).to_hex().to_string()),
                    Value::from(text.len() as u64),
                )
            }
        };
        files.push(json!({
            "path": f.dest,
            "role": f.role.label(),
            "bytes": bytes,
            "blake3": blake3,
            "mode": if f.executable { "755" } else { "644" },
        }));
    }
    let licenses: Vec<&str> = plan
        .files
        .iter()
        .filter(|f| f.role == Role::License)
        .map(|f| f.dest.as_str())
        .collect();
    let build = match &plan.build {
        Some(b) => json!({"tool": b.tool, "command": b.display()}),
        None => json!({"tool": "prebuilt", "from": plan.binary_dir.display().to_string()}),
    };
    let symbols: Vec<Value> = plan
        .symbols
        .iter()
        .map(|s| json!({"path": format!("symbols/{}", s.dest), "bytes": Value::Null, "blake3": Value::Null}))
        .collect();
    Ok(json!({
        "schema": SCHEMA,
        "kind": KIND,
        "dry_run": true,
        "bundle": plan.bundle,
        "target": plan.target.name(),
        "triple": plan.target.triple(),
        "engine_version": engine_version(),
        "git": git,
        "profile": plan.profile,
        "build": build,
        "glibc_floor": plan.target.glibc_floor().map(dist_binary::version_text),
        "binaries": Vec::<Value>::new(),
        "dx12": dx12_json(plan),
        "files": files,
        "licenses": licenses,
        "runtime_dirs": runtime_dirs_json(plan.target),
        "symbols": symbols,
        "warnings": Vec::<String>::new(),
        "archive": plan.archive.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()),
        "missing": missing,
    }))
}

/// The text of a generated file (`source` is `None`): the README or a launcher script.
fn generated_text(file: &PlanFile, readme_text: &str) -> String {
    match file.role {
        Role::Launcher => {
            let bin = file
                .dest
                .strip_prefix("run-")
                .and_then(|n| n.strip_suffix(".cmd"))
                .unwrap_or(&file.dest);
            launcher_script(bin)
        }
        _ => readme_text.to_string(),
    }
}

/// The manifest's `dx12` object: how Direct3D 12 gets its shader compiler in this bundle.
#[must_use]
pub fn dx12_json(plan: &Plan) -> Value {
    let shipped: Vec<&str> = plan
        .files
        .iter()
        .filter(|f| f.role == Role::Dxc)
        .map(|f| f.dest.as_str())
        .collect();
    if plan.target != Target::Windows {
        return Value::Null;
    }
    if shipped.is_empty() {
        json!({
            "compiler": "engine",
            "files": Vec::<&str>::new(),
            "note": "no dxcompiler.dll or dxil.dll in the bundle: Direct3D 12 uses the exe's static DXC \
                     when it was built with the static-dxc feature, otherwise FXC (Shader Model 5.1); \
                     Vulkan is tried first either way",
        })
    } else {
        json!({
            "compiler": "bundled_dxc",
            "files": shipped,
            "note": "dxcompiler.dll and dxil.dll sit next to the exe; Direct3D 12 compiles with DXC \
                     (Shader Model 6); Vulkan is tried first",
        })
    }
}

/// The `runtime_dirs` object of the manifest.
#[must_use]
pub fn runtime_dirs_json(target: Target) -> Value {
    let mut map = serde_json::Map::new();
    for dir in target.runtime_dirs() {
        map.insert(dir.key.to_string(), json!({"path": dir.path, "what": dir.what}));
    }
    Value::Object(map)
}

/// The result of the binary checks on one staged binary.
#[derive(Debug, Clone, PartialEq)]
pub struct BinaryCheck {
    /// The manifest's `binaries[]` entry.
    pub entry: Value,
    /// Problems that fail the bundle.
    pub problems: Vec<String>,
    /// Facts the tester should know (printed and in the README).
    pub warnings: Vec<String>,
}

/// Checks one binary for `target`: a Windows exe's machine and DLL imports (`bundled`: lower-case
/// file names shipped next to it), a Deck binary's machine, glibc floor and libraries.
///
/// # Errors
/// The file is not a PE (Windows) or ELF (Deck) image.
pub fn check_binary(target: Target, file: &str, bytes: &[u8], bundled: &[String]) -> Result<BinaryCheck> {
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    let entry = match target {
        Target::Windows => {
            let pe = dist_binary::read_pe(bytes).with_context(|| format!("{file}: reading the PE header"))?;
            if pe.machine != 0x8664 {
                problems.push(format!(
                    "{file}: machine {:#06x}, expected x64 (0x8664)",
                    pe.machine
                ));
            }
            let mut imports = Vec::new();
            let mut msvc = Vec::new();
            for (dll, delayed) in pe
                .imports
                .iter()
                .map(|d| (d, false))
                .chain(pe.delay_imports.iter().map(|d| (d, true)))
            {
                let class = dist_binary::classify_dll(dll, bundled);
                match class {
                    DllClass::Unknown if !delayed => problems.push(format!(
                        "{file} imports {dll}, which is not part of Windows, the Visual C++ runtime or the \
                         GPU driver, and is not in the bundle: the exe will not start without it (if it is \
                         a Windows DLL, add it to WINDOWS_SYSTEM_DLLS in xtask/src/dist_binary.rs)"
                    )),
                    DllClass::Unknown => warnings.push(format!(
                        "{file} delay-loads {dll}, which is not part of Windows and not in the bundle: the \
                         feature that uses it fails when it is missing"
                    )),
                    DllClass::MsvcRuntime => msvc.push(dll.clone()),
                    _ => {}
                }
                imports.push(json!({"dll": dll, "class": class.label(), "delay_load": delayed}));
            }
            if !msvc.is_empty() {
                warnings.push(format!(
                    "{file} needs the Microsoft Visual C++ Redistributable (x64) for {}; most gaming PCs \
                     have it. Build with --crt-static to drop the dependency",
                    msvc.join(", ")
                ));
            }
            json!({
                "file": file,
                "machine": format!("{:#06x}", pe.machine),
                "subsystem": pe.subsystem_label(),
                "imports": imports,
                "msvc_runtime": if msvc.is_empty() { "static" } else { "dynamic" },
            })
        }
        Target::Deck => {
            let elf =
                dist_binary::read_elf(bytes).with_context(|| format!("{file}: reading the ELF header"))?;
            if elf.machine != 62 {
                problems.push(format!(
                    "{file}: ELF machine {}, expected x86-64 (62)",
                    elf.machine
                ));
            }
            let floor = dist_binary::STEAM_RUNTIME_GLIBC;
            if let Some(max) = elf.max_glibc()
                && max > floor.as_slice()
            {
                problems.push(format!(
                    "{file} needs glibc {}, newer than the Steam Linux Runtime's {}: build with \
                     cargo zigbuild --target x86_64-unknown-linux-gnu.{}",
                    dist_binary::version_text(max),
                    dist_binary::version_text(&floor),
                    dist_binary::version_text(&floor),
                ));
            }
            let outside = elf.outside_runtime();
            let glibc_max = elf.max_glibc().map(dist_binary::version_text);
            if !outside.is_empty() {
                warnings.push(format!(
                    "{file} links {}, which the Steam Linux Runtime 3.0 does not provide: it runs in \
                     Desktop Mode only if SteamOS has them",
                    outside.join(", ")
                ));
            }
            json!({
                "file": file,
                "machine": elf.machine,
                "needed": elf.needed,
                "glibc_max": glibc_max,
                "glibc_floor": dist_binary::version_text(&floor),
                "outside_runtime": outside,
            })
        }
    };
    Ok(BinaryCheck {
        entry,
        problems,
        warnings,
    })
}

/// Where the engine's source code is published, named in every bundle README (MPL-2.0 § 3.2,
/// ADR-10225): the public repository (owner decision 2026-11-20). `regia package` has the same setting
/// (`regia_cli` `package_legal::ENGINE_SOURCE_URL`).
pub const ENGINE_SOURCE_URL: &str = "https://github.com/Regia-Engine/Regia-Engine";

/// The bundle's `README.txt`.
#[must_use]
pub fn readme(plan: &Plan, git: &str, warnings: &[String]) -> String {
    let target = plan.target;
    let sep = if target == Target::Windows { "\\" } else { "/" };
    let mut text = String::new();
    let platform = match target {
        Target::Windows => "Windows 10 or 11 (x64)",
        Target::Deck => "the Steam Deck (SteamOS, Desktop or Game Mode)",
    };
    text.push_str(&format!(
        "Regia Engine {} test bundle for {platform}\nprofile {}, git {git}, target {}\n\n",
        engine_version(),
        plan.profile,
        plan.target.triple()
    ));
    text.push_str("Contents\n");
    for f in &plan.files {
        let path = f.dest.replace('/', sep);
        text.push_str(&format!("  {path:<40} {}\n", f.role.label()));
    }
    text.push_str(&format!(
        "  {MANIFEST_FILE:<40} every file with its size and BLAKE3 hash\n\n\
         Check a copy on a machine with the Regia Engine repository:\n  cargo xtask dist --verify <this folder, or the .{}>\n\n",
        target.archive_ext()
    ));
    text.push_str(&format!(
        "License\n  Regia Engine is under the Mozilla Public License 2.0 (licenses/LICENSE). Its source code\n  \
         is available at {ENGINE_SOURCE_URL} (MPL-2.0 section 3.2).\n\n"
    ));
    text.push_str(match target {
        Target::Windows => "Where the programs write (<exe> is the program name without .exe)\n",
        Target::Deck => "Where the programs write (<exe> is the program name)\n",
    });
    for dir in target.runtime_dirs() {
        text.push_str(&format!(
            "  {:<24} {}\n  {:<24} {}\n",
            dir.key, dir.path, "", dir.what
        ));
    }
    text.push('\n');
    if !warnings.is_empty() {
        text.push_str("Notes\n");
        for w in warnings {
            text.push_str(&format!("  - {w}\n"));
        }
        text.push('\n');
    }
    let has_lab = plan.bins.iter().any(|b| b.bin == "vfx_lab");
    match target {
        Target::Windows => {
            text.push_str(
                "Running\n  Unzip the folder anywhere (not inside Program Files) and open a terminal in it.\n",
            );
            let launchers: Vec<String> = plan
                .files
                .iter()
                .filter(|f| f.role == Role::Launcher)
                .map(|f| f.dest.clone())
                .collect();
            if !launchers.is_empty() {
                text.push_str(&format!(
                    "  Or double-click a launcher ({}): it runs the program from this folder with native\n  \
                     crash capture on, passes its arguments through and waits when the program fails.\n",
                    launchers.join(", ")
                ));
            }
            if has_lab {
                text.push_str(
                    "  Turn on native crash capture first:\n    \
                     PowerShell: $env:REGIA_NATIVE_CRASH=1\n    \
                     cmd:        set REGIA_NATIVE_CRASH=1\n  \
                     .\\vfx_lab.exe --deck-check --minutes 1 --cap 60 --out deck-check\n  \
                     That is a one-minute measured run; it writes summary.json and deck_check.json to deck-check.\n",
                );
            }
        }
        Target::Deck => {
            text.push_str(
                "Running\n  In Desktop Mode: tar xf the archive in your home folder and open Konsole in the folder.\n",
            );
            if has_lab {
                text.push_str(
                    "  ./vfx_lab --deck-check --out ~/deck-check\n  \
                     For Game Mode, add vfx_lab as a non-Steam game with the launch options\n  \
                     --deck-check --fullscreen --out /home/deck/deck-check-gm\n  \
                     The full test list is in the Regia Engine book, Steam Deck page.\n",
                );
            }
        }
    }
    if target == Target::Windows {
        let dxc = plan.files.iter().any(|f| f.role == Role::Dxc);
        text.push_str(if dxc {
            "Direct3D 12\n  dxcompiler.dll and dxil.dll are next to the exe: Direct3D 12 compiles shaders with DXC\n  \
             (Shader Model 6). Keep the two files together. Vulkan is tried first.\n"
        } else {
            "Direct3D 12\n  No dxcompiler.dll or dxil.dll is bundled: Direct3D 12 uses the exe's built-in DXC when\n  \
             it was built with the static-dxc feature, otherwise FXC. Vulkan is tried first.\n"
        });
        text.push_str("Symbols\n  The .pdb files are not in this folder; they are kept in the symbols folder next to the archive.\n\n");
    }
    text.push_str(
        "  When something goes wrong, send back the --out folder and the crash_dumps and logs folders above.\n",
    );
    text
}

/// BLAKE3 (hex) and size of a file, streamed.
fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((hasher.finalize().to_hex().to_string(), size))
}

/// The revision a bundle records: `explicit` (`--git`) when it is given and not blank, else
/// [`git_revision`] of `root`.
///
/// # Errors
/// `dist_git_invalid` when `explicit` is blank or holds whitespace or a control character.
pub fn bundle_git(explicit: Option<&str>, root: &Path) -> Result<String> {
    match explicit {
        None => Ok(git_revision(root)),
        Some(rev) => {
            let rev = rev.trim();
            if rev.is_empty() || rev.chars().any(|c| c.is_whitespace() || c.is_control()) {
                bail!("dist_git_invalid: --git {rev:?} is not a revision (one word, no spaces)");
            }
            Ok(rev.to_string())
        }
    }
}

/// The git revision of the repository (`REGIA_GIT_HASH` first), or `unknown`.
#[must_use]
pub fn git_revision(root: &Path) -> String {
    if let Ok(hash) = std::env::var("REGIA_GIT_HASH")
        && !hash.trim().is_empty()
    {
        return hash.trim().to_string();
    }
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string())
}

/// The archive timestamp: `SOURCE_DATE_EPOCH` when set (reproducible builds), else now.
fn archive_mtime() -> u64 {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        })
}

/// Writes `bytes` to `path` through `<path>.tmp` and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming {}", tmp.display()))
}

/// Sets `0o755` / `0o644` on Unix hosts (Windows has no mode bits; the archive carries them).
fn set_mode(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting the mode of {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable);
    }
    Ok(())
}

/// Marker file `stage` writes into a bundle folder while it fills it, removed once the manifest is
/// written: a run that failed half way can be cleared by the next one.
const STAGING_MARKER: &str = ".regia-dist-staging";

/// Removes an earlier bundle folder, but only one this command wrote (it has a manifest or the
/// staging marker) or an empty one: `dist` never deletes a folder it did not make.
fn clear_bundle_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let empty = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .next()
        .is_none();
    if !empty && !dir.join(MANIFEST_FILE).is_file() && !dir.join(STAGING_MARKER).is_file() {
        bail!(
            "{} exists and is not a dist bundle (no {MANIFEST_FILE}); move it away or pass another --out",
            dir.display()
        );
    }
    fs::remove_dir_all(dir).with_context(|| format!("removing the old bundle {}", dir.display()))
}

/// What [`stage`] produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Staged {
    /// The bundle folder.
    pub dir: PathBuf,
    /// The archive, when one was written.
    pub archive: Option<PathBuf>,
    /// The manifest as written.
    pub manifest: Value,
    /// Notes for the tester (also in the README and printed).
    pub warnings: Vec<String>,
}

/// Stages a planned bundle: copies the files, runs the binary checks, writes the README, the
/// manifest and the archive. The build (if any) must already have run.
///
/// # Errors
/// A missing binary or license, a failed binary check, or IO.
pub fn stage(plan: &Plan, git: &str, mtime: u64) -> Result<Staged> {
    let dir = plan.out.join(&plan.bundle);
    clear_bundle_dir(&dir)?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(dir.join(STAGING_MARKER), b"").with_context(|| format!("writing {STAGING_MARKER}"))?;

    // Copy every sourced file; a missing binary or license fails here.
    for f in &plan.files {
        let Some(source) = &f.source else { continue };
        if !source.is_file() {
            bail!(
                "{} is missing (needed for {} in the bundle)",
                source.display(),
                f.dest
            );
        }
        let dest = dir.join(&f.dest);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::copy(source, &dest).with_context(|| format!("copying {}", source.display()))?;
        set_mode(&dest, f.executable)?;
    }

    // Binary checks on the staged copies.
    let bundled: Vec<String> = plan
        .files
        .iter()
        .filter_map(|f| f.dest.rsplit('/').next())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut binaries = Vec::new();
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    for f in plan.files.iter().filter(|f| f.role == Role::Binary) {
        let bytes = fs::read(dir.join(&f.dest)).with_context(|| format!("reading the staged {}", f.dest))?;
        let check = check_binary(plan.target, &f.dest, &bytes, &bundled)?;
        binaries.push(check.entry);
        problems.extend(check.problems);
        warnings.extend(check.warnings);
    }
    if !problems.is_empty() {
        bail!(
            "the bundle fails its binary checks (left unfinished at {}):\n  {}",
            dir.display(),
            problems.join("\n  ")
        );
    }

    // The DLL set and the DXC pair must be x64 images.
    for f in plan
        .files
        .iter()
        .filter(|f| matches!(f.role, Role::Dll | Role::Dxc))
    {
        let bytes = fs::read(dir.join(&f.dest)).with_context(|| format!("reading the staged {}", f.dest))?;
        match dist_binary::read_pe(&bytes) {
            Ok(pe) if pe.machine == 0x8664 => {}
            Ok(pe) => problems.push(format!(
                "{}: machine {:#06x}, expected x64 (0x8664)",
                f.dest, pe.machine
            )),
            Err(e) => problems.push(format!("{}: not a PE image ({e})", f.dest)),
        }
    }
    if !problems.is_empty() {
        bail!(
            "the bundle fails its binary checks (left unfinished at {}):\n  {}",
            dir.display(),
            problems.join("\n  ")
        );
    }

    let readme_text = readme(plan, git, &warnings);
    for f in plan.files.iter().filter(|f| f.source.is_none()) {
        let text = generated_text(f, &readme_text);
        write_atomic(&dir.join(&f.dest), text.as_bytes())?;
    }

    // Symbols next to the bundle, when the build wrote them.
    let mut symbols = Vec::new();
    for s in &plan.symbols {
        let Some(source) = &s.source else { continue };
        if !source.is_file() {
            continue;
        }
        let symbols_dir = plan.out.join("symbols");
        fs::create_dir_all(&symbols_dir).with_context(|| format!("creating {}", symbols_dir.display()))?;
        let dest = symbols_dir.join(&s.dest);
        fs::copy(source, &dest).with_context(|| format!("copying {}", source.display()))?;
        let (blake3, bytes) = hash_file(&dest)?;
        symbols.push(json!({"path": format!("symbols/{}", s.dest), "bytes": bytes, "blake3": blake3}));
    }

    let sorted: BTreeMap<&str, &PlanFile> = plan.files.iter().map(|f| (f.dest.as_str(), f)).collect();
    let mut files = Vec::new();
    for f in sorted.into_values() {
        let (blake3, bytes) = hash_file(&dir.join(&f.dest))?;
        files.push(json!({
            "path": f.dest,
            "role": f.role.label(),
            "bytes": bytes,
            "blake3": blake3,
            "mode": if f.executable { "755" } else { "644" },
        }));
    }
    let licenses: Vec<&str> = plan
        .files
        .iter()
        .filter(|f| f.role == Role::License)
        .map(|f| f.dest.as_str())
        .collect();
    let build = match &plan.build {
        Some(b) => json!({"tool": b.tool, "command": b.display()}),
        None => json!({"tool": "prebuilt", "from": plan.binary_dir.display().to_string()}),
    };
    let manifest = json!({
        "schema": SCHEMA,
        "kind": KIND,
        "bundle": plan.bundle,
        "target": plan.target.name(),
        "triple": plan.target.triple(),
        "engine_version": engine_version(),
        "git": git,
        "profile": plan.profile,
        "build": build,
        "glibc_floor": plan.target.glibc_floor().map(dist_binary::version_text),
        "binaries": binaries,
        "dx12": dx12_json(plan),
        "files": files,
        "licenses": licenses,
        "runtime_dirs": runtime_dirs_json(plan.target),
        "symbols": symbols,
        "warnings": warnings,
    });
    let text = serde_json::to_string_pretty(&manifest).context("serializing the manifest")?;
    write_atomic(&dir.join(MANIFEST_FILE), format!("{text}\n").as_bytes())?;
    fs::remove_file(dir.join(STAGING_MARKER)).with_context(|| format!("removing {STAGING_MARKER}"))?;

    let archive = match &plan.archive {
        Some(path) => {
            let entries = archive_entries(plan, &dir);
            match plan.target {
                Target::Windows => dist_archive::write_zip(path, &entries, mtime)?,
                Target::Deck => dist_archive::write_tar(path, &entries, mtime)?,
            }
            Some(path.clone())
        }
        None => None,
    };
    Ok(Staged {
        dir,
        archive,
        manifest,
        warnings,
    })
}

/// The archive entries of a staged bundle: the top folder, each subfolder once, then the files in
/// path order, the manifest last.
fn archive_entries(plan: &Plan, dir: &Path) -> Vec<Entry> {
    let mut paths: Vec<(&str, bool)> = plan
        .files
        .iter()
        .map(|f| (f.dest.as_str(), f.executable))
        .collect();
    paths.sort();
    let mut entries = vec![Entry::dir(&plan.bundle)];
    let mut dirs: Vec<String> = Vec::new();
    for (path, _) in &paths {
        let Some((parent, _)) = path.rsplit_once('/') else {
            continue;
        };
        let mut sub = String::new();
        for part in parent.split('/') {
            if !sub.is_empty() {
                sub.push('/');
            }
            sub.push_str(part);
            if !dirs.contains(&sub) {
                dirs.push(sub.clone());
            }
        }
    }
    dirs.sort();
    for sub in &dirs {
        entries.push(Entry::dir(&format!("{}/{sub}", plan.bundle)));
    }
    for (path, executable) in paths {
        entries.push(Entry::file(
            &format!("{}/{path}", plan.bundle),
            &dir.join(path),
            executable,
        ));
    }
    entries.push(Entry::file(
        &format!("{}/{MANIFEST_FILE}", plan.bundle),
        &dir.join(MANIFEST_FILE),
        false,
    ));
    entries
}

/// One file as found by [`verify`]: size, BLAKE3 and Unix mode (when known).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    bytes: u64,
    blake3: String,
    mode: Option<u32>,
}

/// The files of a bundle folder (relative `/` paths), the manifest excluded.
fn scan_dir(dir: &Path) -> Result<BTreeMap<String, Found>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Found>) -> Result<()> {
        let mut children: Vec<PathBuf> = fs::read_dir(dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        children.sort();
        for path in children {
            if path.is_dir() {
                walk(root, &path, out)?;
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .with_context(|| format!("{} is outside {}", path.display(), root.display()))?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let (blake3, bytes) = hash_file(&path)?;
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                Some(fs::metadata(&path)?.permissions().mode() & 0o777)
            };
            #[cfg(not(unix))]
            let mode = None;
            out.insert(rel, Found { bytes, blake3, mode });
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out)?;
    Ok(out)
}

/// Reads a bundle (folder, `.zip` or `.tar`): its manifest and its other files.
fn load_bundle(path: &Path) -> Result<(Value, BTreeMap<String, Found>)> {
    if path.is_dir() {
        let text = fs::read_to_string(path.join(MANIFEST_FILE))
            .with_context(|| format!("{} has no {MANIFEST_FILE}", path.display()))?;
        let manifest: Value = serde_json::from_str(&text).context("parsing the manifest")?;
        let mut files = scan_dir(path)?;
        files.remove(MANIFEST_FILE);
        return Ok((manifest, files));
    }
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let members = match path.extension().and_then(|e| e.to_str()) {
        Some("zip") => dist_archive::list_zip(&bytes)?,
        Some("tar") => dist_archive::list_tar(&bytes)?,
        _ => bail!("{}: expected a bundle folder, a .zip or a .tar", path.display()),
    };
    // The first path component of the first member: zips have no directory entries.
    let top = members
        .first()
        .map(|m| m.name.split('/').next().unwrap_or_default().to_string())
        .filter(|t| !t.is_empty())
        .context("empty archive")?;
    let prefix = format!("{top}/");
    let mut manifest = None;
    let mut files = BTreeMap::new();
    for member in members {
        if member.name.ends_with('/') {
            continue;
        }
        let Some(rel) = member.name.strip_prefix(&prefix) else {
            bail!("{} is outside the archive's top folder {top}", member.name);
        };
        if rel == MANIFEST_FILE {
            manifest = Some(serde_json::from_slice::<Value>(&member.data).context("parsing the manifest")?);
            continue;
        }
        files.insert(
            rel.to_string(),
            Found {
                bytes: member.data.len() as u64,
                blake3: blake3::hash(&member.data).to_hex().to_string(),
                mode: member.mode.map(|m| m & 0o777),
            },
        );
    }
    let manifest = manifest.with_context(|| format!("the archive has no {top}/{MANIFEST_FILE}"))?;
    if manifest["bundle"].as_str() != Some(top.as_str()) {
        bail!(
            "the archive's top folder {top} is not the manifest's bundle {}",
            manifest["bundle"]
        );
    }
    Ok((manifest, files))
}

/// Checks a bundle folder or archive against its manifest: the schema, every listed file present
/// with its size and hash, no unlisted file, the executable bit of Deck binaries, at least one
/// license, and the binary checks again on the binaries. Returns the problems (empty = pass).
///
/// # Errors
/// The bundle or its manifest cannot be read at all.
pub fn verify(path: &Path) -> Result<Vec<String>> {
    verify_against(path, None)
}

/// Verifies a bundle and optionally requires the exact manifest of another staged copy.
pub(crate) fn verify_against(path: &Path, expected: Option<&Value>) -> Result<Vec<String>> {
    let (manifest, found) = load_bundle(path)?;
    let mut problems = Vec::new();
    if expected.is_some_and(|expected| expected != &manifest) {
        problems.push(format!(
            "demo_package_manifest: {} has a different bundle manifest",
            path.display()
        ));
    }
    if manifest["schema"].as_u64() != Some(SCHEMA) || manifest["kind"].as_str() != Some(KIND) {
        problems.push(format!(
            "manifest schema {} / kind {}: this xtask reads schema {SCHEMA}, kind {KIND}",
            manifest["schema"], manifest["kind"]
        ));
        return Ok(problems);
    }
    if manifest["dry_run"].as_bool() == Some(true) {
        problems.push(format!(
            "{MANIFEST_FILE} is a --dry-run preview, not the manifest of a staged bundle: run \
             `cargo xtask dist` without --dry-run"
        ));
        return Ok(problems);
    }
    let Some(target) = manifest["target"].as_str().and_then(Target::from_name) else {
        problems.push(format!("unknown target {}", manifest["target"]));
        return Ok(problems);
    };
    let listed = manifest["files"].as_array().cloned().unwrap_or_default();
    let mut names = Vec::new();
    for file in &listed {
        let Some(name) = file["path"].as_str() else {
            problems.push(format!("a files[] entry has no path: {file}"));
            continue;
        };
        names.push(name.to_string());
        let Some(actual) = found.get(name) else {
            problems.push(format!("{name}: listed in the manifest but missing"));
            continue;
        };
        if file["bytes"].as_u64() != Some(actual.bytes) {
            problems.push(format!(
                "{name}: {} bytes, the manifest says {}",
                actual.bytes, file["bytes"]
            ));
        } else if file["blake3"].as_str() != Some(actual.blake3.as_str()) {
            problems.push(format!("{name}: contents differ from the manifest (BLAKE3)"));
        }
        if target == Target::Deck
            && file["mode"].as_str() == Some("755")
            && let Some(mode) = actual.mode
            && mode & 0o111 == 0
        {
            problems.push(format!(
                "{name}: not executable (mode {mode:o}); the Deck cannot start it"
            ));
        }
    }
    for name in found.keys() {
        if !names.contains(name) {
            problems.push(format!("{name}: in the bundle but not in the manifest"));
        }
    }
    let licenses = manifest["licenses"].as_array().map_or(0, Vec::len);
    if licenses == 0 {
        problems.push("the manifest lists no license file".to_string());
    }
    // Re-run the binary checks on what is actually there (folder bundles only: an archive's
    // members were hashed, which already ties them to the checked staging copy).
    if path.is_dir() {
        let bundled: Vec<String> = names
            .iter()
            .filter_map(|n| n.rsplit('/').next())
            .map(str::to_ascii_lowercase)
            .collect();
        for file in listed.iter().filter(|f| f["role"].as_str() == Some("binary")) {
            let Some(name) = file["path"].as_str() else {
                continue;
            };
            let Ok(bytes) = fs::read(path.join(name)) else {
                continue;
            };
            match check_binary(target, name, &bytes, &bundled) {
                Ok(check) => problems.extend(check.problems),
                Err(e) => problems.push(format!("{e:#}")),
            }
        }
    }
    Ok(problems)
}

/// Runs `cargo xtask dist`.
///
/// # Errors
/// A failed plan, build, binary check or IO.
pub fn run(args: &Args, root: &Path) -> Result<ExitCode> {
    if let Some(path) = &args.verify {
        let problems = verify(path)?;
        if problems.is_empty() {
            println!("dist --verify {}: ok", path.display());
            return Ok(ExitCode::SUCCESS);
        }
        for p in &problems {
            eprintln!("error: {p}");
        }
        eprintln!("dist --verify: {} problem(s)", problems.len());
        return Ok(ExitCode::FAILURE);
    }
    let target_dir = std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let plan = plan(args, root, &target_dir, std::env::consts::OS)?;
    let git = bundle_git(args.git.as_deref(), root)?;
    if args.dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&plan_json(&plan)).context("serializing the plan")?
        );
        if let Some(path) = &args.manifest_out {
            let manifest = preview_manifest(&plan, &git)?;
            let text = serde_json::to_string_pretty(&manifest).context("serializing the manifest preview")?;
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            }
            write_atomic(path, format!("{text}\n").as_bytes())?;
            let missing = manifest["missing"].as_array().map_or(0, Vec::len);
            eprintln!(
                "dist: manifest preview {} ({missing} file(s) not built yet)",
                path.display()
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(build) = &plan.build {
        println!("dist: {}", build.display());
        // The cargo that runs xtask (`CARGO`), so a nested build uses the same toolchain.
        let program = std::env::var_os("CARGO").unwrap_or_else(|| build.program.clone().into());
        let status = Command::new(program)
            .args(&build.args)
            .current_dir(root)
            .status()
            .with_context(|| format!("running {}", build.display()))?;
        if !status.success() {
            let hint = match build.tool {
                "cargo-xwin" => " (needs `cargo install cargo-xwin`)",
                "cargo-zigbuild" => " (needs `cargo install cargo-zigbuild` and zig 0.14+ on PATH)",
                _ => "",
            };
            bail!("the build failed{hint}: {}", build.display());
        }
    }
    let staged = stage(&plan, &git, archive_mtime())?;
    for w in &staged.warnings {
        eprintln!("WARNING: {w}");
    }
    println!("dist: bundle {}", staged.dir.display());
    if let Some(archive) = &staged.archive {
        let bytes = fs::metadata(archive).map_or(0, |m| m.len());
        println!(
            "dist: archive {} ({} MiB)",
            archive.display(),
            bytes / (1024 * 1024)
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(target: Target) -> Args {
        Args {
            target: Some(target),
            profile: "release".to_string(),
            ..Args::default()
        }
    }

    /// ADR-5419: `--notices` replaces the source of `licenses/THIRD_PARTY_NOTICES.md` and nothing else.
    #[test]
    fn a_given_notices_file_replaces_the_engine_wide_notices() -> Result<()> {
        let root = Path::new("/repo");
        let plain = plan(&args(Target::Deck), root, &root.join("target"), "linux")?;
        let source = |p: &Plan, dest: &str| {
            p.files
                .iter()
                .find(|f| f.dest == dest)
                .and_then(|f| f.source.clone())
        };
        assert_eq!(
            source(&plain, "licenses/THIRD_PARTY_NOTICES.md"),
            Some(root.join("THIRD_PARTY_NOTICES.md"))
        );
        let mut a = args(Target::Deck);
        a.notices = Some(PathBuf::from("/repo/docs/demo/THIRD_PARTY_NOTICES-demo.md"));
        let demo = plan(&a, root, &root.join("target"), "linux")?;
        assert_eq!(
            source(&demo, "licenses/THIRD_PARTY_NOTICES.md"),
            a.notices.clone()
        );
        assert_eq!(source(&demo, "licenses/LICENSE"), Some(root.join("LICENSE")));
        assert_eq!(plain.files.len(), demo.files.len());
        Ok(())
    }

    /// ADR-5419: the feature switches reach the cargo command line.
    #[test]
    fn the_feature_switches_reach_the_build_command() -> Result<()> {
        let root = Path::new("/repo");
        let mut a = args(Target::Windows);
        let plain = plan(&a, root, &root.join("target"), "linux")?;
        let plain = plain.build.expect("a build").args;
        assert!(!plain.iter().any(|x| x.contains("features")), "{plain:?}");
        a.no_default_features = true;
        a.features = Some("demo".to_string());
        let demo = plan(&a, root, &root.join("target"), "linux")?
            .build
            .expect("a build")
            .args;
        assert!(demo.contains(&"--no-default-features".to_string()), "{demo:?}");
        let at = demo.iter().position(|x| x == "--features").expect("--features");
        assert_eq!(demo.get(at + 1).map(String::as_str), Some("demo"));
        Ok(())
    }

    #[test]
    fn bin_specs_parse() -> Result<()> {
        assert_eq!(
            BinSpec::parse("vfx_lab")?,
            BinSpec {
                package: "vfx_lab".into(),
                bin: "vfx_lab".into()
            }
        );
        assert_eq!(BinSpec::parse("lookdev:do27_chase")?.bin, "do27_chase");
        for bad in ["", ":x", "x:", "a b", "../x", "x;rm"] {
            assert!(BinSpec::parse(bad).is_err(), "{bad:?}");
        }
        Ok(())
    }

    #[test]
    fn windows_builds_with_cargo_xwin_off_windows_and_cargo_on_windows() -> Result<()> {
        let bins = [BinSpec::parse("vfx_lab")?];
        let linux = build_command(Target::Windows, "linux", "release", &bins, false)?;
        assert_eq!(linux.tool, "cargo-xwin");
        assert_eq!(
            linux.display(),
            "cargo xwin build --profile release --target x86_64-pc-windows-msvc -p vfx_lab --bin vfx_lab"
        );
        let native = build_command(Target::Windows, "windows", "release", &bins, false)?;
        assert_eq!(native.tool, "cargo");
        assert_eq!(native.args[0], "build");
        let static_crt = build_command(Target::Windows, "linux", "shipping", &bins, true)?;
        assert!(static_crt.args.contains(&"shipping".to_string()));
        assert_eq!(
            static_crt.args.last().map(String::as_str),
            Some("target.x86_64-pc-windows-msvc.rustflags=[\"-Ctarget-feature=+crt-static\"]")
        );
        Ok(())
    }

    #[test]
    fn deck_builds_against_the_steam_runtime_glibc_on_linux_only() -> Result<()> {
        let bins = [BinSpec::parse("vfx_lab")?, BinSpec::parse("lookdev:do27_chase")?];
        let cmd = build_command(Target::Deck, "linux", "release", &bins, false)?;
        assert_eq!(cmd.tool, "cargo-zigbuild");
        assert_eq!(
            cmd.display(),
            "cargo zigbuild --profile release --target x86_64-unknown-linux-gnu.2.31 -p vfx_lab -p lookdev \
             --bin vfx_lab --bin do27_chase"
        );
        assert!(build_command(Target::Deck, "windows", "release", &bins, false).is_err());
        assert!(build_command(Target::Deck, "linux", "release", &bins, true).is_err());
        Ok(())
    }

    #[test]
    fn profile_dirs_follow_cargo() {
        assert_eq!(profile_dir("dev"), "debug");
        assert_eq!(profile_dir("release"), "release");
        assert_eq!(profile_dir("shipping"), "shipping");
    }

    #[test]
    fn windows_plan_lists_exe_licenses_readme_and_pdb() -> Result<()> {
        let root = Path::new("/repo");
        let plan = plan(&args(Target::Windows), root, &root.join("target"), "linux")?;
        assert_eq!(plan.bundle, format!("regia-windows-{}", engine_version()));
        assert_eq!(
            plan.binary_dir,
            root.join("target/x86_64-pc-windows-msvc/release")
        );
        assert_eq!(plan.out, root.join("target/dist/windows"));
        let dests: Vec<&str> = plan.files.iter().map(|f| f.dest.as_str()).collect();
        assert_eq!(
            dests,
            [
                "vfx_lab.exe",
                "run-vfx_lab.cmd",
                "licenses/LICENSE",
                "licenses/THIRD_PARTY_NOTICES.md",
                "README.txt"
            ]
        );
        assert_eq!(plan.symbols.len(), 1);
        assert_eq!(plan.symbols[0].dest, "vfx_lab.pdb");
        assert_eq!(
            plan.archive,
            Some(root.join(format!(
                "target/dist/windows/regia-windows-{}.zip",
                engine_version()
            )))
        );
        let json = plan_json(&plan);
        assert_eq!(json["triple"], "x86_64-pc-windows-msvc");
        assert_eq!(json["glibc_floor"], Value::Null);
        assert_eq!(
            json["runtime_dirs"]["crash_dumps"]["path"],
            r"%LOCALAPPDATA%\RegiaEngine\<exe>\feedback"
        );
        assert_eq!(
            json["runtime_dirs"]["pipeline_cache"]["path"],
            r"%LOCALAPPDATA%\RegiaEngine\<exe>\pipelines"
        );
        Ok(())
    }

    #[test]
    fn a_given_readme_and_revision_replace_the_generated_ones() -> Result<()> {
        // ADR-5002: `--readme` ships a file as README.txt, `--git` names the binaries' build.
        let root = Path::new("/repo");
        let mut a = args(Target::Windows);
        a.no_launcher = true;
        a.readme = Some(PathBuf::from("/players/README.txt"));
        let plan = plan(&a, root, &root.join("target"), "linux")?;
        let readme = plan
            .files
            .iter()
            .find(|f| f.dest == "README.txt")
            .context("README planned")?;
        assert_eq!(readme.role, Role::Readme);
        assert_eq!(readme.source, Some(PathBuf::from("/players/README.txt")));
        assert!(
            !plan.files.iter().any(|f| f.role == Role::Launcher),
            "--no-launcher writes no run-<bin>.cmd"
        );
        assert_eq!(bundle_git(Some(" b394124ea "), root)?, "b394124ea");
        for bad in ["", "  ", "a b", "a\nb"] {
            let err = bundle_git(Some(bad), root).expect_err("not a revision");
            assert!(err.to_string().starts_with("dist_git_invalid"), "{err}");
        }
        Ok(())
    }

    #[test]
    fn deck_plan_has_no_exe_suffix_and_a_tar() -> Result<()> {
        let root = Path::new("/repo");
        let mut a = args(Target::Deck);
        a.no_archive = true;
        let plan = plan(&a, root, &root.join("target"), "linux")?;
        assert_eq!(plan.files[0].dest, "vfx_lab");
        assert!(plan.files[0].executable);
        assert!(plan.symbols.is_empty());
        assert_eq!(plan.archive, None);
        assert_eq!(
            plan.binary_dir,
            root.join("target/x86_64-unknown-linux-gnu/release")
        );
        let json = plan_json(&plan);
        assert_eq!(json["glibc_floor"], "2.31");
        assert_eq!(
            json["runtime_dirs"]["crash_dumps"]["path"],
            "~/.local/share/RegiaEngine/<exe>/feedback"
        );
        a.no_archive = false;
        let plan = super::plan(&a, root, &root.join("target"), "linux")?;
        assert!(
            plan.archive
                .is_some_and(|p| p.extension().is_some_and(|e| e == "tar"))
        );
        Ok(())
    }

    #[test]
    fn from_skips_the_build_and_refuses_crt_static() -> Result<()> {
        let root = Path::new("/repo");
        let mut a = args(Target::Deck);
        a.from = Some(PathBuf::from("/prebuilt"));
        // --from works on any host, even where the Deck build cannot run.
        let plan = plan(&a, root, &root.join("target"), "windows")?;
        assert_eq!(plan.build, None);
        assert_eq!(plan.files[0].source, Some(PathBuf::from("/prebuilt/vfx_lab")));
        let mut w = args(Target::Windows);
        w.from = Some(PathBuf::from("/prebuilt"));
        w.crt_static = true;
        assert!(super::plan(&w, root, &root.join("target"), "linux").is_err());
        Ok(())
    }

    #[test]
    fn duplicate_bins_and_missing_target_are_refused() {
        let root = Path::new("/repo");
        let mut a = args(Target::Windows);
        a.bins = vec!["vfx_lab".into(), "other:vfx_lab".into()];
        assert!(plan(&a, root, root, "linux").is_err());
        assert!(plan(&Args::default(), root, root, "linux").is_err());
    }

    #[test]
    fn command_line_parses() -> Result<()> {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            dist: Args,
        }
        let cli = Cli::try_parse_from([
            "dist",
            "--target",
            "deck",
            "--bin",
            "vfx_lab",
            "--bin",
            "lookdev:do27_chase",
            "--profile",
            "shipping",
            "--from",
            "/built",
            "--no-archive",
            "--dry-run",
        ])?;
        assert_eq!(cli.dist.target, Some(Target::Deck));
        assert_eq!(cli.dist.bins, ["vfx_lab", "lookdev:do27_chase"]);
        assert_eq!(cli.dist.profile, "shipping");
        assert_eq!(cli.dist.from, Some(PathBuf::from("/built")));
        assert!(cli.dist.no_archive && cli.dist.dry_run && !cli.dist.crt_static);
        let defaults = Cli::try_parse_from(["dist", "--verify", "bundle.zip"])?;
        assert_eq!(defaults.dist.profile, "release");
        assert_eq!(defaults.dist.target, None);
        assert_eq!(defaults.dist.verify, Some(PathBuf::from("bundle.zip")));
        assert!(Cli::try_parse_from(["dist", "--target", "mac"]).is_err());
        Ok(())
    }

    #[test]
    fn every_runtime_dir_key_is_on_both_targets() {
        let keys = |t: Target| t.runtime_dirs().iter().map(|d| d.key).collect::<Vec<_>>();
        assert_eq!(keys(Target::Windows), keys(Target::Deck));
        for key in [
            "user",
            "logs",
            "crash_dumps",
            "cache",
            "pipeline_cache",
            "shipped_pipeline_caches",
        ] {
            assert!(keys(Target::Windows).contains(&key), "{key}");
        }
        assert_eq!(Target::from_name("deck"), Some(Target::Deck));
        assert_eq!(Target::from_name("mac"), None);
    }
}
