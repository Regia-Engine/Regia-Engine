// SPDX-License-Identifier: MPL-2.0
//! `regia package --platform linux --container <profile>`: build the game binary inside a build
//! container whose glibc is the oldest the players have (C-26, ADR-8835;
//! `docs/specs/21-tooling-and-packaging.md` § 4).
//!
//! A binary links against the glibc of the machine that builds it and does not start on an older
//! one. cargo-zigbuild (`--glibc`) relinks against an older glibc's symbol list; a container
//! profile instead builds in a real older distribution, so C and C++ dependencies (and their
//! `libstdc++`) also come from it:
//!
//! | Profile | Base image | glibc |
//! |---|---|---|
//! | `steam-sniper` | Steam Linux Runtime 3.0 "sniper" SDK (Debian 11 based) | 2.31 |
//! | `debian12` | Debian 12 "bookworm" | 2.36 |
//!
//! Rules (ADR-8835): the base image is named by digest, never by a moving tag; the container tool
//! (podman, else docker, or `REGIA_CONTAINER_TOOL`) is found at run time and a missing tool is the
//! error `container_tool_missing`, not a panic; the workspace and any extra source directories are
//! mounted read-only at their host paths; the cargo target directory and the cargo home are per
//! profile (`<target>/container/<profile>/`), so a container build never shares artifacts with a
//! host build linked against another glibc. The staged binary then goes through the same glibc
//! floor check as every Linux package (`ship_check::check_linux_binary`, read from the ELF bytes).
//!
//! Requirements were taken from two shell build scripts of a game project (absorb row C-26,
//! mark "requirements"; nothing copied): a sniper SDK build with clang as the C compiler (its
//! GCC 10 cannot build some AVX2 intrinsics, and a newer GCC's `libstdc++` would need symbols the
//! runtime lacks), a Debian 12 build, read-only source mounts, a separate cargo home and target
//! directory per container, and a glibc symbol-version gate on the result.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::ValueEnum;

use crate::commands::ship_check::GlibcVersion;
use crate::process;

/// `--container` on a platform other than Linux.
pub const CONTAINER_PLATFORM_UNSUPPORTED: &str = "container_platform_unsupported";
/// The machine running `regia package` is not Linux (container builds mount host paths).
pub const CONTAINER_HOST_UNSUPPORTED: &str = "container_host_unsupported";
/// Neither podman nor docker (nor `REGIA_CONTAINER_TOOL`) is on `PATH`.
pub const CONTAINER_TOOL_MISSING: &str = "container_tool_missing";
/// `REGIA_CONTAINER_TOOL` names something other than `podman` or `docker`.
pub const CONTAINER_TOOL_UNKNOWN: &str = "container_tool_unknown";
/// `--container-image` names a moving reference (`:latest`, no tag, no digest).
pub const CONTAINER_IMAGE_UNPINNED: &str = "container_image_unpinned";
/// `--container-image` names a local image that is not present.
pub const CONTAINER_IMAGE_MISSING: &str = "container_image_missing";
/// `--glibc` disagrees with the profile's glibc, or a `--steam` package asks for a newer glibc.
pub const CONTAINER_GLIBC_CONFLICT: &str = "container_glibc_conflict";
/// The workspace has no `Cargo.lock` (the sources are mounted read-only; cargo cannot write one).
pub const CONTAINER_LOCKFILE_MISSING: &str = "container_lockfile_missing";
/// A `--container-mount` directory does not exist.
pub const CONTAINER_MOUNT_MISSING: &str = "container_mount_missing";
/// The build inside the container (or the toolchain image build) failed.
pub const CONTAINER_BUILD_FAILED: &str = "container_build_failed";

/// A container build error with its stable code (lowercase; `regia package --json` reports it as
/// `error.code`).
#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ContainerError {
    /// Stable code, one of the `CONTAINER_*` constants.
    pub code: &'static str,
    /// What went wrong and how to fix it.
    pub message: String,
}

impl ContainerError {
    /// An error with `code`.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// A build-container profile of `regia package --container`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub enum ContainerProfile {
    /// Steam Linux Runtime 3.0 "sniper" SDK: glibc 2.31, what Steam and the Steam Deck run native
    /// Linux games in. C and C++ dependencies build with clang.
    #[value(name = "steam-sniper")]
    SteamSniper,
    /// Debian 12 "bookworm": glibc 2.36, for distributions from about 2023 on.
    #[value(name = "debian12")]
    Debian12,
}

/// The pinned facts of a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileSpec {
    /// The profile's `--container` name.
    pub name: &'static str,
    /// Base image by digest (`<repository>@sha256:<hex>`).
    pub base_image: &'static str,
    /// The dated tag the digest was taken from (documentation only; never pulled by tag).
    pub base_tag: &'static str,
    /// glibc of the image: the floor of the binaries it builds.
    pub glibc: GlibcVersion,
    /// Environment of the build inside the container.
    pub env: &'static [(&'static str, &'static str)],
    /// Packages the toolchain image installs (`apt-get install`), the engine's Linux build
    /// dependencies; empty when the base image has them.
    pub apt_packages: &'static [&'static str],
}

/// The engine's Linux build dependencies (the same set as the CI's Linux jobs).
const DEBIAN_BUILD_PACKAGES: &[&str] = &[
    "ca-certificates",
    "curl",
    "gcc",
    "g++",
    "libc6-dev",
    "make",
    "pkg-config",
    "libasound2-dev",
    "libudev-dev",
    "libdbus-1-dev",
    "libxkbcommon-dev",
    "libwayland-dev",
];

const SNIPER: ProfileSpec = ProfileSpec {
    name: "steam-sniper",
    base_image: "registry.gitlab.steamos.cloud/steamrt/sniper/sdk@sha256:ee829e4437aa6470b1c094ce52b99f4723ac4b32ccd0c8e13e8750a9949d728c",
    base_tag: "3.0.20261110.268881",
    glibc: GlibcVersion { major: 2, minor: 31 },
    // The SDK's GCC 10 cannot build some AVX2 intrinsics used by a texture codec dependency, and
    // its newer GCC's libstdc++ headers emit GLIBCXX symbols the runtime lacks; clang builds the
    // intrinsics and links the SDK's GCC 10 libstdc++.
    env: &[("CC", "clang"), ("CXX", "clang++")],
    // The SDK ships the toolchain and the development headers.
    apt_packages: &[],
};

const DEBIAN12: ProfileSpec = ProfileSpec {
    name: "debian12",
    base_image: "docker.io/library/debian@sha256:268fb24c39bca92d326dd83ea3abebe4a3f7751c804ffef23bb6d857dd343ff8",
    base_tag: "bookworm-20261104",
    glibc: GlibcVersion { major: 2, minor: 36 },
    env: &[],
    apt_packages: DEBIAN_BUILD_PACKAGES,
};

impl ContainerProfile {
    /// Every profile, in `--container` help order.
    #[cfg(test)]
    pub const ALL: [Self; 2] = [Self::SteamSniper, Self::Debian12];

    /// The profile's pinned facts.
    #[must_use]
    pub fn spec(self) -> &'static ProfileSpec {
        match self {
            Self::SteamSniper => &SNIPER,
            Self::Debian12 => &DEBIAN12,
        }
    }

    /// The `--container` name.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.spec().name
    }

    /// glibc of the profile's image: the floor of what it builds.
    #[must_use]
    pub fn glibc(self) -> GlibcVersion {
        self.spec().glibc
    }

    /// The toolchain image `regia package` builds from the base image for `rust_version`:
    /// `localhost/regia-build-<profile>:<rust>-<first 12 digest hex digits>`, so another Rust
    /// version or another base digest is another image.
    #[must_use]
    pub fn toolchain_image(self, rust_version: &str) -> String {
        let digest = self
            .spec()
            .base_image
            .rsplit_once("@sha256:")
            .map_or("", |(_, hex)| &hex[..hex.len().min(12)]);
        format!("localhost/regia-build-{}:{rust_version}-{digest}", self.name())
    }

    /// The Containerfile of the toolchain image: the pinned base, the profile's packages, and a
    /// minimal rustup install of `rust_version` readable by every user (docker runs the build as
    /// the invoking user).
    #[must_use]
    pub fn containerfile(self, rust_version: &str) -> String {
        let spec = self.spec();
        let mut out = format!(
            "# Generated by `regia package --container {}` (ADR-8835). Base: {} ({}).\n\
             FROM {}\n\
             ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH\n",
            spec.name, spec.base_tag, spec.glibc, spec.base_image
        );
        if !spec.apt_packages.is_empty() {
            out.push_str(&format!(
                "RUN apt-get update && apt-get install -y --no-install-recommends {} && rm -rf /var/lib/apt/lists/*\n",
                spec.apt_packages.join(" ")
            ));
        }
        out.push_str(&format!(
            "RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path \
             --profile minimal --default-toolchain {rust_version} && chmod -R a+rX /usr/local/rustup /usr/local/cargo\n"
        ));
        out
    }
}

/// The container front end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerTool {
    /// podman (rootless: the container's root is the invoking user).
    Podman,
    /// docker (runs the build as the invoking user's uid and gid).
    Docker,
}

impl ContainerTool {
    /// The program name.
    #[must_use]
    pub fn program(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
        }
    }
}

/// Finds the container tool: `REGIA_CONTAINER_TOOL` (`podman` or `docker`) when set, else podman,
/// else docker, on `PATH` (`on_path`).
///
/// # Errors
///
/// [`CONTAINER_TOOL_UNKNOWN`] for another `REGIA_CONTAINER_TOOL` value, [`CONTAINER_TOOL_MISSING`]
/// when the chosen or every tool is absent.
pub fn detect_tool(
    choice: Option<&OsStr>,
    on_path: impl Fn(&str) -> bool,
) -> Result<ContainerTool, ContainerError> {
    let install = "install podman (or docker) and re-run; `--glibc <version>` (cargo-zigbuild) builds \
                   against an older glibc without a container";
    if let Some(choice) = choice.filter(|c| !c.is_empty()) {
        let tool = match choice.to_str() {
            Some("podman") => ContainerTool::Podman,
            Some("docker") => ContainerTool::Docker,
            _ => {
                return Err(ContainerError::new(
                    CONTAINER_TOOL_UNKNOWN,
                    format!("REGIA_CONTAINER_TOOL={choice:?}: expected `podman` or `docker`"),
                ));
            }
        };
        if !on_path(tool.program()) {
            return Err(ContainerError::new(
                CONTAINER_TOOL_MISSING,
                format!(
                    "REGIA_CONTAINER_TOOL={} is not on PATH: {install}",
                    tool.program()
                ),
            ));
        }
        return Ok(tool);
    }
    [ContainerTool::Podman, ContainerTool::Docker]
        .into_iter()
        .find(|tool| on_path(tool.program()))
        .ok_or_else(|| {
            ContainerError::new(
                CONTAINER_TOOL_MISSING,
                format!("container builds need podman or docker on PATH: {install}"),
            )
        })
}

/// Refuses an image reference that can move: an image must be pinned by digest
/// (`<name>@sha256:<64 hex>`), or be a local image (`localhost/<name>:<tag>`) with a tag other than
/// `latest`.
///
/// # Errors
///
/// [`CONTAINER_IMAGE_UNPINNED`].
pub fn check_image_pinned(reference: &str) -> Result<(), ContainerError> {
    if let Some((_, hex)) = reference.rsplit_once("@sha256:") {
        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(());
        }
        return Err(ContainerError::new(
            CONTAINER_IMAGE_UNPINNED,
            format!("{reference:?}: a digest is `@sha256:` and 64 hex digits"),
        ));
    }
    if let Some(local) = reference.strip_prefix("localhost/") {
        // The tag follows the last `:` after the last `/`.
        let name = local.rsplit('/').next().unwrap_or(local);
        if let Some((_, tag)) = name.rsplit_once(':')
            && !tag.is_empty()
            && tag != "latest"
        {
            return Ok(());
        }
    }
    Err(ContainerError::new(
        CONTAINER_IMAGE_UNPINNED,
        format!(
            "{reference:?} can change under the same name: pin it by digest (`<image>@sha256:<hex>`) or use a \
             local image with a version tag (`localhost/<name>:<tag>`, not `latest`)"
        ),
    ))
}

/// One bind mount of the container run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// Directory on the host.
    pub host: PathBuf,
    /// Where the container sees it (the same path: build paths, remapping and messages agree).
    pub container: PathBuf,
    /// Mounted read-only (sources) or read-write (target directory, cargo home).
    pub read_only: bool,
}

/// The per-profile directories under the host's cargo target directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileDirs {
    /// `<target>/container/<profile>`.
    pub root: PathBuf,
    /// `<root>/target`: the container build's `CARGO_TARGET_DIR`.
    pub target: PathBuf,
    /// `<root>/cargo-home`: the container build's `CARGO_HOME` (registry and git caches).
    pub cargo_home: PathBuf,
}

impl ProfileDirs {
    /// The directories of `profile` under the host cargo target directory `target_dir`.
    #[must_use]
    pub fn new(target_dir: &Path, profile: ContainerProfile) -> Self {
        let root = target_dir.join("container").join(profile.name());
        Self {
            target: root.join("target"),
            cargo_home: root.join("cargo-home"),
            root,
        }
    }
}

/// Everything one container build runs: the tool, the image, the mounts and the cargo call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerPlan {
    /// The image the build runs in (the toolchain image or `--container-image`).
    pub image: String,
    /// The cargo workspace root (mounted read-only).
    pub workspace: PathBuf,
    /// Extra read-only source directories (`--container-mount`: path dependencies outside the
    /// workspace).
    pub extra_mounts: Vec<PathBuf>,
    /// The per-profile target directory and cargo home.
    pub dirs: ProfileDirs,
    /// The arguments after `cargo`.
    pub cargo_args: Vec<String>,
    /// Environment of the build (the profile's, cargo's, and the build identity).
    pub env: Vec<(String, String)>,
    /// `uid:gid` to run as (docker; podman's rootless root already is the invoking user).
    pub user: Option<String>,
}

impl ContainerPlan {
    /// The bind mounts, in mount order: the workspace and extra directories read-only, then the
    /// per-profile target directory and cargo home read-write (a target directory inside the
    /// workspace is mounted over the read-only workspace).
    #[must_use]
    pub fn mounts(&self) -> Vec<Mount> {
        let ro = std::iter::once(&self.workspace)
            .chain(self.extra_mounts.iter())
            .map(|dir| Mount {
                host: dir.clone(),
                container: dir.clone(),
                read_only: true,
            });
        let rw = [&self.dirs.target, &self.dirs.cargo_home]
            .into_iter()
            .map(|dir| Mount {
                host: dir.clone(),
                container: dir.clone(),
                read_only: false,
            });
        ro.chain(rw).collect()
    }

    /// The arguments after the tool's program: `run --rm`, the mounts, the working directory, the
    /// environment, the image, and `cargo <args>`.
    #[must_use]
    pub fn run_args(&self) -> Vec<String> {
        let mut args = vec!["run".to_string(), "--rm".to_string()];
        if let Some(user) = &self.user {
            args.push("--user".into());
            args.push(user.clone());
        }
        for mount in self.mounts() {
            args.push("--volume".into());
            let mode = if mount.read_only { "ro" } else { "rw" };
            args.push(format!(
                "{}:{}:{mode}",
                mount.host.display(),
                mount.container.display()
            ));
        }
        args.push("--workdir".into());
        args.push(self.workspace.display().to_string());
        for (name, value) in &self.env {
            args.push("--env".into());
            args.push(format!("{name}={value}"));
        }
        args.push(self.image.clone());
        args.push("cargo".into());
        args.extend(self.cargo_args.iter().cloned());
        args
    }

    /// Where the build writes `bin` for cargo profile directory `profile_dir` (`shipping`, `debug`).
    #[must_use]
    pub fn binary_path(&self, profile_dir: &str, bin: &str) -> PathBuf {
        self.dirs.target.join(profile_dir).join(bin)
    }
}

/// The environment of a container build: cargo's directories, no color, the host's
/// `CARGO_BUILD_JOBS` when set, the profile's compilers, and `identity` (the build identity
/// variables of a shipping build).
#[must_use]
pub fn build_env(
    profile: ContainerProfile,
    dirs: &ProfileDirs,
    jobs: Option<String>,
    identity: &[(&'static str, String)],
) -> Vec<(String, String)> {
    let mut env = vec![
        ("CARGO_HOME".to_string(), dirs.cargo_home.display().to_string()),
        ("CARGO_TARGET_DIR".to_string(), dirs.target.display().to_string()),
        ("CARGO_TERM_COLOR".to_string(), "never".to_string()),
    ];
    if let Some(jobs) = jobs.filter(|j| !j.is_empty()) {
        env.push(("CARGO_BUILD_JOBS".to_string(), jobs));
    }
    env.extend(
        profile
            .spec()
            .env
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string())),
    );
    env.extend(
        identity
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone())),
    );
    env
}

/// The cargo arguments of a container build: the host call's arguments with `--locked` added
/// (the workspace is read-only, so cargo must not rewrite `Cargo.lock`).
#[must_use]
pub fn container_cargo_args(mut host_args: Vec<String>) -> Vec<String> {
    if !host_args.iter().any(|a| a == "--locked" || a == "--frozen") {
        host_args.push("--locked".to_string());
    }
    host_args
}

/// What `regia package --container` was given.
#[derive(Debug, Clone)]
pub struct ContainerRequest<'a> {
    /// The profile.
    pub profile: ContainerProfile,
    /// `--container-image`: a ready image with cargo, instead of the generated toolchain image.
    pub image: Option<&'a str>,
    /// `--container-mount` directories.
    pub mounts: &'a [PathBuf],
}

/// The `uid:gid` owning `dir` (the workspace: the user whose files the build writes beside).
#[cfg(unix)]
fn owner_of(dir: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(dir).ok()?;
    Some(format!("{}:{}", meta.uid(), meta.gid()))
}

#[cfg(not(unix))]
fn owner_of(_dir: &Path) -> Option<String> {
    None
}

/// The host toolchain's version (`rustc --version` → `1.99.0`): the toolchain image installs the
/// same compiler.
fn host_rust_version() -> Result<String> {
    let (out, _) = process::output(
        Command::new("rustc").arg("--version"),
        "Install Rust from https://rustup.rs",
    )?;
    out.split_whitespace()
        .nth(1)
        .map(str::to_string)
        .context("`rustc --version` printed no version")
}

/// Whether the tool has `image` locally (`<tool> image inspect`; never pulls).
pub fn image_present(tool: ContainerTool, image: &str) -> bool {
    Command::new(tool.program())
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Validates a request before the (long) cook and build: the host is Linux, every mount exists,
/// `--container-image` is pinned.
///
/// # Errors
///
/// [`CONTAINER_HOST_UNSUPPORTED`], [`CONTAINER_MOUNT_MISSING`], [`CONTAINER_IMAGE_UNPINNED`].
pub fn validate(request: &ContainerRequest<'_>) -> Result<(), ContainerError> {
    if !cfg!(target_os = "linux") {
        return Err(ContainerError::new(
            CONTAINER_HOST_UNSUPPORTED,
            "container builds run on a Linux host (they mount the host's directories); use `--glibc` there \
             or build on Linux",
        ));
    }
    for dir in request.mounts {
        if !dir.is_dir() {
            return Err(ContainerError::new(
                CONTAINER_MOUNT_MISSING,
                format!("--container-mount {}: no such directory", dir.display()),
            ));
        }
    }
    if let Some(image) = request.image {
        check_image_pinned(image)?;
    }
    Ok(())
}

/// The host side of a container build: what the host cargo call would have been.
#[derive(Debug, Clone)]
pub struct HostCall {
    /// The cargo workspace root (mounted read-only).
    pub workspace: PathBuf,
    /// The host cargo target directory; the per-profile directories go under it.
    pub target_dir: PathBuf,
    /// The host build's arguments after `cargo` (`package::cargo_build_call`).
    pub args: Vec<String>,
    /// The build identity variables of a shipping build (`REGIA_GIT_HASH`, ...).
    pub identity: Vec<(&'static str, String)>,
    /// Cargo's profile directory (`shipping`, `debug`).
    pub profile_dir: &'static str,
    /// The binary's file name.
    pub bin: String,
}

/// Builds the package binary in the profile's container and returns its path on the host.
///
/// # Errors
///
/// Any `CONTAINER_*` error, or the toolchain image build failing.
pub fn build(
    request: &ContainerRequest<'_>,
    call: HostCall,
    on_path: impl Fn(&str) -> bool,
) -> Result<PathBuf> {
    let HostCall {
        workspace,
        target_dir,
        args: host_args,
        identity,
        profile_dir,
        bin,
    } = call;
    let (workspace, target_dir) = (workspace.as_path(), target_dir.as_path());
    validate(request)?;
    let tool = detect_tool(
        regia_core::brand::env_var_os("REGIA_CONTAINER_TOOL").as_deref(),
        on_path,
    )?;
    if !workspace.join("Cargo.lock").is_file() {
        return Err(ContainerError::new(
            CONTAINER_LOCKFILE_MISSING,
            format!(
                "{} has no Cargo.lock; the container mounts the sources read-only, so run `cargo generate-lockfile` \
                 (or any host build) first and commit the lock file",
                workspace.display()
            ),
        )
        .into());
    }
    let profile = request.profile;
    let dirs = ProfileDirs::new(target_dir, profile);
    let image = match request.image {
        Some(image) => {
            if image.starts_with("localhost/") && !image_present(tool, image) {
                return Err(ContainerError::new(
                    CONTAINER_IMAGE_MISSING,
                    format!("--container-image {image} is not present in {}", tool.program()),
                )
                .into());
            }
            image.to_string()
        }
        None => {
            let rust = host_rust_version()?;
            let image = profile.toolchain_image(&rust);
            if !image_present(tool, &image) {
                build_toolchain_image(tool, profile, &rust, &image, &dirs.root)?;
            }
            image
        }
    };
    for dir in [&dirs.target, &dirs.cargo_home] {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let plan = ContainerPlan {
        image,
        workspace: workspace.to_path_buf(),
        extra_mounts: request.mounts.to_vec(),
        cargo_args: container_cargo_args(host_args),
        env: build_env(
            profile,
            &dirs,
            regia_core::brand::env_var("CARGO_BUILD_JOBS").ok(),
            &identity,
        ),
        user: match tool {
            ContainerTool::Docker => owner_of(workspace),
            ContainerTool::Podman => None,
        },
        dirs,
    };
    outln!(
        "container: building in {} ({}, glibc {})",
        plan.image,
        profile.name(),
        profile.glibc()
    );
    process::run(
        Command::new(tool.program()).args(plan.run_args()),
        "install podman or docker",
    )
    .map_err(|err| ContainerError::new(CONTAINER_BUILD_FAILED, format!("{err:#}")))?;
    Ok(plan.binary_path(profile_dir, &bin))
}

/// Builds the profile's toolchain image (pulls the pinned base image once) from a Containerfile
/// written to `<profile root>/image/`.
fn build_toolchain_image(
    tool: ContainerTool,
    profile: ContainerProfile,
    rust: &str,
    image: &str,
    profile_root: &Path,
) -> Result<()> {
    let context = profile_root.join("image");
    std::fs::create_dir_all(&context).with_context(|| format!("creating {}", context.display()))?;
    let file = context.join("Containerfile");
    let tmp = context.join("Containerfile.tmp");
    std::fs::write(&tmp, profile.containerfile(rust))
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &file).with_context(|| format!("writing {}", file.display()))?;
    outln!(
        "container: building the toolchain image {image} from {} (rustc {rust}; pulls the base once)",
        profile.spec().base_image
    );
    let mut cmd = Command::new(tool.program());
    cmd.arg("build")
        .arg("--tag")
        .arg(image)
        .arg("--file")
        .arg(&file)
        .arg(&context);
    process::run(&mut cmd, "install podman or docker")
        .map_err(|err| ContainerError::new(CONTAINER_BUILD_FAILED, format!("toolchain image: {err:#}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(tool: ContainerTool, profile: ContainerProfile) -> ContainerPlan {
        let workspace = PathBuf::from("/work/game");
        let dirs = ProfileDirs::new(&workspace.join("target"), profile);
        ContainerPlan {
            image: profile.toolchain_image("1.99.0"),
            workspace,
            extra_mounts: vec![PathBuf::from("/work/engine")],
            cargo_args: container_cargo_args(
                ["build", "--profile", "shipping", "-p", "night_train"]
                    .map(str::to_string)
                    .to_vec(),
            ),
            env: build_env(
                profile,
                &dirs,
                Some("8".into()),
                &[("REGIA_BUILD_PROFILE", "shipping".into())],
            ),
            user: (tool == ContainerTool::Docker).then(|| "1000:1000".to_string()),
            dirs,
        }
    }

    /// C-26 acceptance: each profile builds against its glibc floor from a digest-pinned image, with
    /// the sources read-only and a per-profile target directory and cargo home.
    #[test]
    fn container_profile_builds_glibc_floor() {
        assert_eq!(
            ContainerProfile::SteamSniper.glibc(),
            GlibcVersion { major: 2, minor: 31 }
        );
        assert_eq!(
            ContainerProfile::SteamSniper.glibc(),
            crate::commands::ship_check::STEAM_RUNTIME_GLIBC,
            "the sniper profile builds for the Steam runtime's glibc"
        );
        assert_eq!(
            ContainerProfile::Debian12.glibc(),
            GlibcVersion { major: 2, minor: 36 }
        );
        for profile in ContainerProfile::ALL {
            let spec = profile.spec();
            check_image_pinned(spec.base_image).expect("base image pinned by digest");
            assert!(!spec.base_image.contains(":latest"), "{}", spec.base_image);
            let containerfile = profile.containerfile("1.99.0");
            assert!(
                containerfile.contains(&format!("FROM {}\n", spec.base_image)),
                "{containerfile}"
            );
            assert!(
                containerfile.contains("--default-toolchain 1.99.0"),
                "{containerfile}"
            );
            let image = profile.toolchain_image("1.99.0");
            assert!(image.starts_with(&format!("localhost/regia-build-{}:1.99.0-", profile.name())));
            check_image_pinned(&image).expect("the toolchain image has a version tag");

            let plan = plan(ContainerTool::Podman, profile);
            let mounts = plan.mounts();
            assert_eq!(
                mounts[0],
                Mount {
                    host: "/work/game".into(),
                    container: "/work/game".into(),
                    read_only: true
                },
                "the workspace is read-only at its host path"
            );
            assert_eq!(
                mounts[1],
                Mount {
                    host: "/work/engine".into(),
                    container: "/work/engine".into(),
                    read_only: true
                }
            );
            let target = PathBuf::from(format!("/work/game/target/container/{}/target", profile.name()));
            let cargo_home = PathBuf::from(format!(
                "/work/game/target/container/{}/cargo-home",
                profile.name()
            ));
            assert_eq!(
                mounts[2..],
                [
                    Mount {
                        host: target.clone(),
                        container: target.clone(),
                        read_only: false
                    },
                    Mount {
                        host: cargo_home.clone(),
                        container: cargo_home.clone(),
                        read_only: false
                    },
                ]
            );
            let args = plan.run_args();
            let line = args.join(" ");
            assert!(
                line.starts_with("run --rm --volume /work/game:/work/game:ro "),
                "{line}"
            );
            assert!(line.contains("--volume /work/engine:/work/engine:ro"), "{line}");
            assert!(
                line.contains(&format!("--volume {0}:{0}:rw", target.display())),
                "{line}"
            );
            assert!(line.contains("--workdir /work/game"), "{line}");
            assert!(
                line.contains(&format!("--env CARGO_TARGET_DIR={}", target.display())),
                "{line}"
            );
            assert!(
                line.contains(&format!("--env CARGO_HOME={}", cargo_home.display())),
                "{line}"
            );
            assert!(line.contains("--env CARGO_BUILD_JOBS=8"), "{line}");
            assert!(line.contains("--env REGIA_BUILD_PROFILE=shipping"), "{line}");
            assert!(
                !line.contains("--user"),
                "rootless podman needs no --user: {line}"
            );
            assert!(
                line.ends_with(&format!(
                    "{} cargo build --profile shipping -p night_train --locked",
                    plan.image
                )),
                "{line}"
            );
            assert_eq!(
                plan.binary_path("shipping", "night_train"),
                target.join("shipping/night_train")
            );
        }
        // The sniper SDK builds C and C++ with clang (GLIBCXX of the runtime); Debian 12 with gcc.
        let sniper = plan(ContainerTool::Podman, ContainerProfile::SteamSniper)
            .run_args()
            .join(" ");
        assert!(sniper.contains("--env CC=clang --env CXX=clang++"), "{sniper}");
        let debian = plan(ContainerTool::Podman, ContainerProfile::Debian12)
            .run_args()
            .join(" ");
        assert!(!debian.contains("CC=clang"), "{debian}");
        assert!(
            ContainerProfile::Debian12
                .containerfile("1.99.0")
                .contains("libasound2-dev")
        );
        // Profiles never share a target directory.
        assert_ne!(
            ProfileDirs::new(Path::new("/t"), ContainerProfile::SteamSniper).target,
            ProfileDirs::new(Path::new("/t"), ContainerProfile::Debian12).target
        );
        // docker runs as the workspace owner so the target directory is not root-owned.
        let docker = plan(ContainerTool::Docker, ContainerProfile::Debian12).run_args();
        assert_eq!(docker[..4], ["run", "--rm", "--user", "1000:1000"]);
    }

    #[test]
    fn the_container_tool_is_found_at_run_time() {
        let both = |_: &str| true;
        let docker_only = |tool: &str| tool == "docker";
        let none = |_: &str| false;
        assert_eq!(detect_tool(None, both).expect("podman"), ContainerTool::Podman);
        assert_eq!(
            detect_tool(None, docker_only).expect("docker"),
            ContainerTool::Docker
        );
        assert_eq!(
            detect_tool(Some(OsStr::new("docker")), both).expect("chosen"),
            ContainerTool::Docker
        );
        assert_eq!(
            detect_tool(Some(OsStr::new("")), docker_only).expect("empty is unset"),
            ContainerTool::Docker
        );
        let missing = detect_tool(None, none).expect_err("no tool");
        assert_eq!(missing.code, CONTAINER_TOOL_MISSING);
        assert!(missing.to_string().starts_with("container_tool_missing: "));
        assert_eq!(
            detect_tool(Some(OsStr::new("podman")), docker_only)
                .expect_err("chosen tool absent")
                .code,
            CONTAINER_TOOL_MISSING
        );
        assert_eq!(
            detect_tool(Some(OsStr::new("nerdctl")), both)
                .expect_err("unknown")
                .code,
            CONTAINER_TOOL_UNKNOWN
        );
    }

    #[test]
    fn container_images_must_be_pinned() {
        let digest = format!("docker.io/library/debian@sha256:{}", "a".repeat(64));
        check_image_pinned(&digest).expect("digest");
        check_image_pinned("localhost/regia-bullseye-rust:1.99").expect("local version tag");
        check_image_pinned("localhost:5000/x@sha256:").expect_err("empty digest");
        for moving in [
            "debian:12",
            "debian",
            "registry.gitlab.steamos.cloud/steamrt/sniper/sdk:latest",
            "localhost/regia-build:latest",
            "localhost/regia-build",
            "docker.io/library/debian@sha256:abc",
        ] {
            assert_eq!(
                check_image_pinned(moving).expect_err(moving).code,
                CONTAINER_IMAGE_UNPINNED,
                "{moving}"
            );
        }
    }

    #[test]
    fn container_cargo_args_lock_the_read_only_workspace() {
        let args = container_cargo_args(vec!["build".into(), "--frozen".into()]);
        assert_eq!(args, ["build", "--frozen"]);
        let args = container_cargo_args(vec!["build".into()]);
        assert_eq!(args, ["build", "--locked"]);
    }

    /// The real container build, end to end, where it can run without pulling anything:
    /// `REGIA_CONTAINER_TEST_IMAGE` names an image already present in podman or docker that has cargo
    /// (`REGIA_CONTAINER_TEST_PROFILE`: `steam-sniper`, the default, or `debian12`, the profile whose
    /// glibc floor the image must meet). It builds a one-file crate in a temporary read-only workspace
    /// through [`build`] and reads the binary's newest `GLIBC_` version. Without the variable, or
    /// without a container tool, there is nothing to run here and the test says so (farm hosts never
    /// pull images; ADR-8835).
    #[cfg(target_os = "linux")]
    #[test]
    fn container_profile_real_build_meets_the_glibc_floor() {
        let Some(image) = std::env::var("REGIA_CONTAINER_TEST_IMAGE")
            .ok()
            .filter(|v| !v.is_empty())
        else {
            eprintln!("REGIA_CONTAINER_TEST_IMAGE unset: no real container build here");
            return;
        };
        let on_path = |tool: &str| {
            std::env::var_os("PATH")
                .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()))
        };
        let tool = detect_tool(std::env::var_os("REGIA_CONTAINER_TOOL").as_deref(), on_path)
            .expect("REGIA_CONTAINER_TEST_IMAGE is set, so a container tool must be installed");
        assert!(
            image_present(tool, &image),
            "{image} is not present in {}; this test never pulls",
            tool.program()
        );
        let profile = match std::env::var("REGIA_CONTAINER_TEST_PROFILE").as_deref() {
            Ok("debian12") => ContainerProfile::Debian12,
            _ => ContainerProfile::SteamSniper,
        };
        let dir = regia_test_utils::unique_temp_dir("cli-container-build");
        let workspace = dir.join("ws");
        std::fs::create_dir_all(workspace.join("src")).expect("mkdir");
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"floor_probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .expect("write Cargo.toml");
        std::fs::write(
            workspace.join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"floor_probe\"\nversion = \"0.1.0\"\n",
        )
        .expect("write Cargo.lock");
        std::fs::write(
            workspace.join("src/main.rs"),
            "fn main() { let t = std::thread::spawn(|| std::env::args().count()); println!(\"{}\", t.join().unwrap_or(0)); }\n",
        )
        .expect("write main.rs");
        let request = ContainerRequest {
            profile,
            image: Some(&image),
            mounts: &[],
        };
        let call = HostCall {
            workspace: workspace.clone(),
            target_dir: dir.join("target"),
            args: ["build", "--profile", "release", "-p", "floor_probe"]
                .map(str::to_string)
                .to_vec(),
            identity: Vec::new(),
            profile_dir: "release",
            bin: "floor_probe".to_string(),
        };
        let binary = build(&request, call, on_path).expect("the container build succeeds");
        assert_eq!(
            binary,
            dir.join(format!(
                "target/container/{}/target/release/floor_probe",
                profile.name()
            ))
        );
        let bytes = std::fs::read(&binary).expect("the container wrote the binary");
        let max = crate::commands::elf_versions::max_glibc_needed(&bytes).expect("ELF parses");
        eprintln!("{image} ({}): floor_probe needs GLIBC_{max:?}", profile.name());
        let max = max.expect("a dynamically linked probe needs glibc versions");
        assert!(
            max <= profile.glibc(),
            "built in {image}: needs GLIBC_{max}, above the {} floor {}",
            profile.name(),
            profile.glibc()
        );
        // The sources were read-only: cargo wrote nothing into the workspace.
        assert!(!workspace.join("target").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn requests_are_validated_before_the_build() {
        let missing = [PathBuf::from("/nonexistent/regia-container-mount")];
        let request = ContainerRequest {
            profile: ContainerProfile::Debian12,
            image: None,
            mounts: &missing,
        };
        assert_eq!(
            validate(&request).expect_err("missing mount").code,
            CONTAINER_MOUNT_MISSING
        );
        let request = ContainerRequest {
            profile: ContainerProfile::Debian12,
            image: Some("debian:12"),
            mounts: &[],
        };
        assert_eq!(
            validate(&request).expect_err("moving image").code,
            CONTAINER_IMAGE_UNPINNED
        );
    }
}
