// SPDX-License-Identifier: MIT-0
//! Locate a workspace binary next to this executable, or build it once into that directory.
//!
//! `CARGO_BIN_EXE_<name>` exists only in the package that owns the binary. Tests in other
//! packages (and the lookdev bins) still know the cargo target dir and profile from their own
//! path: a bin is `<target>/<profile>/<bin>`, a test is `<target>/<profile>/deps/<test>-<hash>`,
//! an example is `<target>/<profile>/examples/<example>-<hash>`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The `regia` CLI for this process's target directory and profile.
///
/// `REGIA_CLI` wins when it names a file. Otherwise the binary beside this executable is
/// used. When that file is absent it is built once with
/// `cargo build -p regia_cli --bin regia` (same `--target-dir` and profile).
///
/// # Errors
///
/// `REGIA_CLI` names a missing file, or the on-demand build fails. The error names the
/// missing binary and the command that builds it.
pub(crate) fn ensure_regia_cli() -> Result<PathBuf, String> {
    static CACHED: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    CACHED
        .get_or_init(|| ensure_workspace_bin("regia_cli", "regia"))
        .clone()
}

/// `package`'s `bin`, found beside this executable or built into that profile directory.
///
/// Not itself once-guarded; [`ensure_regia_cli`] caches the regia result.
///
/// # Errors
///
/// The override (when `REGIA_CLI` is the regia bin) is missing, the workspace root cannot
/// be found, or `cargo build` fails. The error names the binary and the build command.
fn ensure_workspace_bin(package: &str, bin: &str) -> Result<PathBuf, String> {
    let file_name = exe_file_name(bin);
    if bin == "regia"
        && let Some(override_path) = std::env::var_os("REGIA_CLI").map(PathBuf::from)
    {
        if override_path.is_file() {
            return Ok(override_path);
        }
        return Err(failure(
            &override_path,
            package,
            bin,
            "REGIA_CLI is set but that file does not exist.",
        ));
    }

    let layout = ArtifactLayout::detect();
    let expected = layout.candidate(&file_name);
    if expected.is_file() {
        return Ok(expected);
    }

    build_into(&layout, package, bin, &expected)
}

/// Cargo target directory plus the profile directory name (`debug`, `release`, `shipping`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtifactLayout {
    target_dir: PathBuf,
    profile_dir: String,
}

impl ArtifactLayout {
    /// Layout of the running executable when it sits in a cargo target directory.
    fn detect() -> Self {
        std::env::current_exe()
            .ok()
            .and_then(|exe| Self::from_exe(&exe))
            .filter(Self::plausible)
            .unwrap_or_else(Self::fallback)
    }

    /// Parse `exe` without checking that the directory is a real cargo target dir.
    fn from_exe(exe: &Path) -> Option<Self> {
        let mut dir = exe.parent()?.to_path_buf();
        if dir
            .file_name()
            .is_some_and(|name| name == "deps" || name == "examples")
            && !dir.pop()
        {
            return None;
        }
        let profile_dir = dir.file_name()?.to_str()?.to_owned();
        if profile_dir.is_empty() || !dir.pop() {
            return None;
        }
        Some(Self {
            target_dir: dir,
            profile_dir,
        })
    }

    /// `<workspace>/target/<dev-or-release>` when the executable path is not a cargo layout.
    fn fallback() -> Self {
        let target_dir = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
            || workspace_root().map_or_else(|_| PathBuf::from("target"), |root| root.join("target")),
            PathBuf::from,
        );
        let profile_dir = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
        .to_owned();
        Self {
            target_dir,
            profile_dir,
        }
    }

    /// Reject paths that only look a bit like a target dir (`/debug/deps/…` must not become
    /// `cargo build --target-dir /`).
    fn plausible(&self) -> bool {
        let named_target = self.target_dir.file_name().is_some_and(|name| name == "target");
        let tagged = self.target_dir.join("CACHEDIR.TAG").is_file();
        if !(named_target || tagged) {
            return false;
        }
        !self.profile_dir.is_empty()
            && !self.profile_dir.contains(['/', '\\'])
            && self.profile_dir != "deps"
            && self.profile_dir != "examples"
    }

    fn candidate(&self, file_name: &str) -> PathBuf {
        self.target_dir.join(&self.profile_dir).join(file_name)
    }

    /// Cargo arguments that write this profile into [`Self::target_dir`].
    fn profile_args(&self) -> Vec<String> {
        match self.profile_dir.as_str() {
            "debug" => Vec::new(),
            "release" => vec!["--release".to_owned()],
            other => vec!["--profile".to_owned(), other.to_owned()],
        }
    }
}

fn exe_file_name(bin: &str) -> String {
    format!("{bin}{}", std::env::consts::EXE_SUFFIX)
}

fn build_into(layout: &ArtifactLayout, package: &str, bin: &str, expected: &Path) -> Result<PathBuf, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let root = workspace_root().map_err(|detail| failure(expected, package, bin, &detail))?;
    let args = cargo_args(layout, package, bin, &root.join("Cargo.toml"));
    let command = format!("cargo {}", args.join(" "));
    eprintln!("building `{bin}` into {}:\n  {command}", expected.display());

    let output = Command::new(&cargo)
        .args(&args)
        .current_dir(&root)
        .output()
        .map_err(|err| {
            failure(
                expected,
                package,
                bin,
                &format!("could not run `{command}`: {err}"),
            )
        })?;

    if output.status.success() && expected.is_file() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            eprint!("{stderr}");
        }
        return Ok(expected.to_path_buf());
    }

    let stdout_owned = String::from_utf8_lossy(&output.stdout);
    let stderr_owned = String::from_utf8_lossy(&output.stderr);
    let stdout = tail(&stdout_owned, 8_000);
    let stderr = tail(&stderr_owned, 8_000);
    Err(failure(
        expected,
        package,
        bin,
        &format!(
            "on-demand build exited {} (`{command}`)\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status
        ),
    ))
}

/// Arguments after `cargo`. The same list is what the message tells a person to run.
fn cargo_args(layout: &ArtifactLayout, package: &str, bin: &str, manifest: &Path) -> Vec<String> {
    let mut args = vec![
        "build".to_owned(),
        "-p".to_owned(),
        package.to_owned(),
        "--bin".to_owned(),
        bin.to_owned(),
        "--manifest-path".to_owned(),
        manifest.display().to_string(),
        "--target-dir".to_owned(),
        layout.target_dir.display().to_string(),
    ];
    args.extend(layout.profile_args());
    args.extend(job_args());
    args
}

fn job_args() -> Vec<String> {
    job_args_from(std::env::var("CARGO_BUILD_JOBS").ok().as_deref())
}

fn job_args_from(value: Option<&str>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let value = value.trim();
    if value.parse::<u32>().ok().is_some_and(|jobs| jobs > 0) {
        vec!["-j".to_owned(), value.to_owned()]
    } else {
        Vec::new()
    }
}

fn failure(path: &Path, package: &str, bin: &str, detail: &str) -> String {
    format!(
        "missing binary `{file}` at {shown}.\n\
         Build it with: cargo build -p {package} --bin {bin}\n\
         {detail}",
        file = exe_file_name(bin),
        shown = path.display(),
    )
}

fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let start = text.floor_char_boundary(text.len() - max);
    &text[start..]
}

fn workspace_root() -> Result<PathBuf, String> {
    let start = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut dir = start.as_path();
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() && is_workspace_manifest(&manifest) {
            return Ok(dir.to_path_buf());
        }
        dir = dir.parent().ok_or_else(|| {
            format!(
                "no workspace Cargo.toml above {} (cannot build the missing binary)",
                start.display()
            )
        })?;
    }
}

fn is_workspace_manifest(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.lines().any(|line| {
        let line = line.trim();
        line == "[workspace]" || line.starts_with("[workspace.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_resolves_to_the_profile_dir() {
        let exe = Path::new("/repo/target/debug/deps/cooked_runtime-hash");
        let layout = ArtifactLayout::from_exe(exe).expect("layout");
        assert_eq!(layout.profile_dir, "debug");
        assert_eq!(layout.target_dir, Path::new("/repo/target"));
        assert!(layout.profile_args().is_empty());
        assert_eq!(
            layout.candidate(&exe_file_name("regia")),
            Path::new("/repo/target/debug").join(exe_file_name("regia"))
        );
    }

    #[test]
    fn release_bin_and_example_use_their_profile_dir() {
        let bin = ArtifactLayout::from_exe(Path::new("/repo/target/release/cooked_runtime")).expect("bin");
        assert_eq!(bin.profile_args(), vec!["--release".to_owned()]);
        assert_eq!(bin.target_dir, Path::new("/repo/target"));

        let example = ArtifactLayout::from_exe(Path::new("/repo/target/debug/examples/sim_smoke-hash"))
            .expect("example");
        assert_eq!(example.profile_dir, "debug");
        assert_eq!(example.target_dir, Path::new("/repo/target"));
    }

    #[test]
    fn custom_profile_and_target_triple_keep_their_directories() {
        let shipping = ArtifactLayout::from_exe(Path::new("/repo/target/shipping/deps/cook_pak_frame-hash"))
            .expect("shipping");
        assert_eq!(
            shipping.profile_args(),
            vec!["--profile".to_owned(), "shipping".to_owned()]
        );
        assert_eq!(shipping.target_dir, Path::new("/repo/target"));

        let triple = ArtifactLayout::from_exe(Path::new(
            "/repo/target/x86_64-unknown-linux-gnu/debug/deps/cook_pak_frame-hash",
        ))
        .expect("triple");
        assert_eq!(triple.profile_dir, "debug");
        assert_eq!(
            triple.target_dir,
            Path::new("/repo/target/x86_64-unknown-linux-gnu")
        );
        assert!(triple.profile_args().is_empty());
    }

    #[test]
    fn a_path_outside_a_target_dir_is_not_plausible() {
        let parsed = ArtifactLayout::from_exe(Path::new("/tmp/cooked_runtime")).expect("parses");
        assert_eq!(parsed.profile_dir, "tmp");
        assert!(!parsed.plausible(), "must not cargo-build into /");

        let rootish = ArtifactLayout {
            target_dir: PathBuf::from("/"),
            profile_dir: "debug".to_owned(),
        };
        assert!(!rootish.plausible());
    }

    #[test]
    fn a_directory_named_target_is_plausible_without_a_tag() {
        let layout = ArtifactLayout {
            target_dir: PathBuf::from("/repo/target"),
            profile_dir: "debug".to_owned(),
        };
        assert!(layout.plausible());
    }

    #[test]
    fn cachedir_tag_marks_a_custom_target_dir() {
        let dir = std::env::temp_dir().join(format!("regia-workspace-bin-{}-tag", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("CACHEDIR.TAG"),
            b"Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .expect("tag");
        let layout = ArtifactLayout {
            target_dir: dir.clone(),
            profile_dir: "weird".to_owned(),
        };
        assert!(layout.plausible());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failure_names_the_binary_and_how_to_build_it() {
        let message = failure(
            Path::new("/repo/target/debug/regia"),
            "regia_cli",
            "regia",
            "on-demand build exited 101",
        );
        assert!(message.contains("missing binary `regia"), "{message}");
        assert!(message.contains("/repo/target/debug/regia"), "{message}");
        assert!(
            message.contains("cargo build -p regia_cli --bin regia"),
            "{message}"
        );
        assert!(message.contains("on-demand build exited 101"), "{message}");
    }

    #[test]
    fn workspace_root_is_the_regia_checkout() {
        let root = workspace_root().expect("workspace");
        assert!(
            root.join("crates/regia_cli/Cargo.toml").is_file(),
            "{}",
            root.display()
        );
        assert!(root.join("Cargo.toml").is_file());
    }

    #[test]
    fn job_args_follow_cargo_build_jobs() {
        assert_eq!(job_args_from(Some("4")), vec!["-j".to_owned(), "4".to_owned()]);
        assert_eq!(job_args_from(Some(" 6 ")), vec!["-j".to_owned(), "6".to_owned()]);
        assert!(job_args_from(None).is_empty());
        assert!(job_args_from(Some("0")).is_empty());
        assert!(job_args_from(Some("nope")).is_empty());
    }
}
