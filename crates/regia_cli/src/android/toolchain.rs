// SPDX-License-Identifier: MPL-2.0
//! Locating the Android toolchain: SDK, NDK, JDK, Gradle and adb.
//!
//! Every lookup is split into a pure `*_from` function (tested) and a wrapper that reads the
//! environment. Failures carry a fix-it message.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::versions::{GRADLE_VERSION, MIN_GRADLE_VERSION};
use crate::process;

/// Parses a dotted numeric version (`8.14.3`, `28.0.13004108`). `None` for anything else
/// (pre-releases such as `8.14-rc-1` are deliberately rejected).
pub fn parse_version(text: &str) -> Option<Vec<u32>> {
    text.split('.').map(|part| part.parse().ok()).collect()
}

/// The user's home directory from the environment.
pub fn home_dir() -> Option<PathBuf> {
    regia_core::brand::env_var_os("HOME")
        .or_else(|| regia_core::brand::env_var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn env_path(name: &str) -> Option<PathBuf> {
    regia_core::brand::env_var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Default SDK location of Android Studio on this host OS, relative to the home directory.
fn default_sdk_under(home: &Path) -> PathBuf {
    if cfg!(windows) {
        home.join("AppData").join("Local").join("Android").join("Sdk")
    } else if cfg!(target_os = "macos") {
        home.join("Library").join("Android").join("sdk")
    } else {
        home.join("Android").join("Sdk")
    }
}

/// Picks the Android SDK: `ANDROID_HOME`, then `ANDROID_SDK_ROOT`, then the Android Studio default.
pub fn find_sdk_from(
    android_home: Option<PathBuf>,
    android_sdk_root: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf> {
    let (sdk, source) = if let Some(p) = android_home {
        (p, "ANDROID_HOME")
    } else if let Some(p) = android_sdk_root {
        (p, "ANDROID_SDK_ROOT")
    } else if let Some(home) = home {
        (default_sdk_under(&home), "the default location")
    } else {
        bail!("Android SDK not found: set ANDROID_HOME to the SDK directory");
    };
    if !sdk.is_dir() {
        bail!(
            "Android SDK not found at {} ({source}). Install it with Android Studio or the command-line \
             tools and set ANDROID_HOME to its directory",
            sdk.display()
        );
    }
    Ok(sdk)
}

/// [`find_sdk_from`] using the process environment.
pub fn find_sdk() -> Result<PathBuf> {
    find_sdk_from(env_path("ANDROID_HOME"), env_path("ANDROID_SDK_ROOT"), home_dir())
}

/// Fails with an `sdkmanager` hint unless `platforms/android-<api>` is installed.
pub fn check_sdk_platform(sdk: &Path, api: u32) -> Result<()> {
    if !sdk.join("platforms").join(format!("android-{api}")).is_dir() {
        bail!(
            "Android SDK platform {api} is not installed in {}. Install it with \
             `sdkmanager \"platforms;android-{api}\"` (or lower target_sdk in Regia.toml)",
            sdk.display()
        );
    }
    Ok(())
}

/// Returns the newest `<sdk>/ndk/<version>` directory, if any.
pub fn newest_ndk_in(sdk: &Path) -> Option<PathBuf> {
    newest_versioned_child(&sdk.join("ndk"), parse_version)
}

/// Picks the NDK: `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`, then the newest under `<sdk>/ndk`.
pub fn find_ndk(sdk: &Path) -> Option<PathBuf> {
    env_path("ANDROID_NDK_HOME")
        .or_else(|| env_path("ANDROID_NDK_ROOT"))
        .filter(|p| p.is_dir())
        .or_else(|| newest_ndk_in(sdk))
}

/// Version of the NDK in `ndk_dir`: `Pkg.Revision` from `source.properties`, else the directory name
/// when it looks like a version.
pub fn ndk_version(ndk_dir: &Path) -> Option<String> {
    let from_properties = std::fs::read_to_string(ndk_dir.join("source.properties"))
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "Pkg.Revision").then(|| value.trim().to_owned())
            })
        });
    from_properties.or_else(|| {
        let name = ndk_dir.file_name()?.to_string_lossy().into_owned();
        parse_version(&name).map(|_| name)
    })
}

fn newest_versioned_child(dir: &Path, version_of: impl Fn(&str) -> Option<Vec<u32>>) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let version = version_of(&entry.file_name().to_string_lossy())?;
            Some((version, entry.path()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, path)| path)
}

/// Gradle launcher script name for the host OS.
fn gradle_script_name() -> &'static str {
    if cfg!(windows) { "gradle.bat" } else { "gradle" }
}

/// Finds a Gradle launcher among wrapper-cached distributions in `dists`
/// (`<dists>/gradle-<ver>-{bin,all}/<hash>/gradle-<ver>/bin/gradle`).
/// Prefers the pinned [`GRADLE_VERSION`], else the newest of the same major version that is at least
/// [`MIN_GRADLE_VERSION`] (the pinned Android Gradle plugin is not validated against other majors).
pub fn cached_gradle_in(dists: &Path) -> Option<PathBuf> {
    let pinned = parse_version(GRADLE_VERSION)?;
    let minimum = parse_version(MIN_GRADLE_VERSION)?;
    let mut candidates: Vec<(Vec<u32>, PathBuf)> = Vec::new();
    for dist in std::fs::read_dir(dists).ok()?.filter_map(|e| e.ok()) {
        let name = dist.file_name().to_string_lossy().into_owned();
        let Some(version_text) = name
            .strip_prefix("gradle-")
            .and_then(|rest| rest.strip_suffix("-bin").or_else(|| rest.strip_suffix("-all")))
        else {
            continue;
        };
        let Some(version) = parse_version(version_text) else {
            continue;
        };
        let Ok(hashes) = std::fs::read_dir(dist.path()) else {
            continue;
        };
        for hash in hashes.filter_map(|e| e.ok()) {
            let script = hash
                .path()
                .join(format!("gradle-{version_text}"))
                .join("bin")
                .join(gradle_script_name());
            if script.is_file() {
                candidates.push((version.clone(), script));
            }
        }
    }
    if let Some((_, script)) = candidates.iter().find(|(v, _)| *v == pinned) {
        return Some(script.clone());
    }
    candidates
        .into_iter()
        .filter(|(v, _)| *v >= minimum && v.first() == pinned.first())
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, s)| s)
}

/// Searches `PATH` for an executable.
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = regia_core::brand::env_var_os("PATH")?;
    find_in_path_from(&path, name, cfg!(windows))
}

/// `find_in_path` over the given `PATH` value. On Windows the executable extensions come before
/// the bare name in each folder, in `CreateProcess`'s default `PATHEXT` order, so an
/// extensionless shim next to `gradle.exe` is not picked.
fn find_in_path_from(path: &std::ffi::OsStr, name: &str, windows: bool) -> Option<PathBuf> {
    let exts: &[&str] = if windows {
        &[".com", ".exe", ".bat", ".cmd", ""]
    } else {
        &[""]
    };
    std::env::split_paths(path).find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{name}{ext}")))
            .find(|candidate| candidate.is_file())
    })
}

/// Locates Gradle: `REGIA_GRADLE`, then `gradle` on `PATH`, then a wrapper-cached distribution under
/// `$GRADLE_USER_HOME` (default `~/.gradle`). No wrapper jar is shipped.
pub fn find_gradle() -> Result<PathBuf> {
    if let Some(explicit) = env_path("REGIA_GRADLE") {
        if !explicit.is_file() {
            bail!(
                "REGIA_GRADLE points to {}, which does not exist",
                explicit.display()
            );
        }
        return Ok(explicit);
    }
    if let Some(on_path) = find_in_path("gradle") {
        return Ok(on_path);
    }
    let gradle_home = env_path("GRADLE_USER_HOME").or_else(|| home_dir().map(|h| h.join(".gradle")));
    if let Some(found) = gradle_home.and_then(|home| cached_gradle_in(&home.join("wrapper").join("dists"))) {
        return Ok(found);
    }
    bail!(
        "Gradle not found. Install Gradle {GRADLE_VERSION} (e.g. `sdk install gradle {GRADLE_VERSION}` or your \
         package manager) and put it on PATH, or set REGIA_GRADLE to the `gradle` launcher"
    )
}

/// Extracts the Java feature version from `java -version` output
/// (`openjdk version "17.0.2"` → 17, `java version "1.8.0_402"` → 8).
pub fn parse_java_major(version_output: &str) -> Option<u32> {
    // JVM environment notices can precede the version and contain their own quoted values.
    let version = version_output.lines().find_map(|line| {
        let line = line.trim_start();
        line.strip_prefix("openjdk version ")
            .or_else(|| line.strip_prefix("java version "))
    })?;
    let start = version.find('"')? + 1;
    let rest = &version[start..];
    let quoted = &rest[..rest.find('"')?];
    let mut parts = quoted.split(['.', '_', '-', '+']);
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

/// Checks that a JDK 17 or newer is available to Gradle (`JAVA_HOME`, else `java` on `PATH`).
pub fn check_java() -> Result<()> {
    let (java, source): (OsString, &str) = match env_path("JAVA_HOME") {
        Some(home) => {
            let exe = if cfg!(windows) { "java.exe" } else { "java" };
            (home.join("bin").join(exe).into_os_string(), "JAVA_HOME")
        }
        None => ("java".into(), "PATH"),
    };
    let fix = "Install JDK 17 (e.g. `pacman -S jdk17-openjdk`) and set JAVA_HOME or put `java` on PATH";
    let (stdout, stderr) = process::output(Command::new(&java).arg("-version"), fix)
        .with_context(|| format!("checking the JDK from {source}"))?;
    let text = format!("{stderr}{stdout}");
    match parse_java_major(&text) {
        Some(major) if major >= 17 => Ok(()),
        Some(major) => {
            bail!("Gradle needs JDK 17 or newer, but the JDK from {source} is version {major}. {fix}")
        }
        None => bail!("cannot read the Java version from `java -version`:\n{text}"),
    }
}

/// Locates `adb`: `<sdk>/platform-tools/adb`, else `adb` on `PATH`.
pub fn find_adb(sdk: Option<&Path>) -> Result<PathBuf> {
    let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
    if let Some(in_sdk) = sdk
        .map(|s| s.join("platform-tools").join(exe))
        .filter(|p| p.is_file())
    {
        return Ok(in_sdk);
    }
    find_in_path("adb").context(
        "adb not found. Install the SDK platform-tools (`sdkmanager platform-tools`) or put adb on PATH",
    )
}

/// Rust target triple of an Android ABI.
pub fn abi_triple(abi: &str) -> Option<&'static str> {
    Some(match abi {
        "arm64-v8a" => "aarch64-linux-android",
        "armeabi-v7a" => "armv7-linux-androideabi",
        "x86_64" => "x86_64-linux-android",
        "x86" => "i686-linux-android",
        _ => return None,
    })
}

/// Triples needed for `abis` that are absent from `rustup target list --installed` output.
pub fn missing_rust_targets(installed: &str, abis: &[String]) -> Vec<&'static str> {
    abis.iter()
        .filter_map(|abi| abi_triple(abi))
        .filter(|triple| !installed.lines().any(|line| line.trim() == *triple))
        .collect()
}

/// Fails with a `rustup target add` hint when the toolchain selected in `workspace_root` (which honours
/// the project's `rust-toolchain.toml`) lacks a standard library for one of `abis`.
/// Without rustup the check is skipped and cargo reports the problem itself.
pub fn check_rust_targets(workspace_root: &Path, abis: &[String]) -> Result<()> {
    let Some(installed) = process::try_output(
        Command::new("rustup")
            .args(["target", "list", "--installed"])
            .current_dir(workspace_root),
    ) else {
        return Ok(());
    };
    let missing = missing_rust_targets(&installed, abis);
    if !missing.is_empty() {
        bail!(
            "the Rust toolchain used in {} has no standard library for {}. Run `rustup target add {}` \
             there (or pin a toolchain that has it in the project's rust-toolchain.toml)",
            workspace_root.display(),
            missing.join(", "),
            missing.join(" ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// CLI2 platform audit 4: on Windows an executable extension wins over an extensionless file
    /// in the same folder (a shell shim named `gradle`), as `CreateProcess` would pick, and `.com`
    /// is probed; elsewhere only the bare name counts.
    #[test]
    fn find_in_path_prefers_exe_over_extensionless_file() {
        let dir = regia_test_utils::unique_temp_dir("toolchain_find_in_path");
        let (first, second) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&first).expect("a");
        std::fs::create_dir_all(&second).expect("b");
        std::fs::write(first.join("tool"), b"#!/bin/sh\n").expect("shim");
        std::fs::write(first.join("tool.exe"), b"MZ").expect("exe");
        std::fs::write(second.join("other.com"), b"MZ").expect("com");
        let path = std::env::join_paths([&first, &second]).expect("join");
        assert_eq!(
            super::find_in_path_from(&path, "tool", true),
            Some(first.join("tool.exe"))
        );
        assert_eq!(
            super::find_in_path_from(&path, "tool", false),
            Some(first.join("tool"))
        );
        assert_eq!(
            super::find_in_path_from(&path, "other", true),
            Some(second.join("other.com"))
        );
        assert_eq!(super::find_in_path_from(&path, "other", false), None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn audit_java_version_ignores_quoted_environment_notices() {
        let output =
            "Picked up JAVA_TOOL_OPTIONS: -Dapp.name=\"Regia\"\nopenjdk version \"17.0.2\" 2022-01-18\n";
        assert_eq!(super::parse_java_major(output), Some(17));
        assert_eq!(
            super::parse_java_major("Picked up _JAVA_OPTIONS: -Dversion=\"99\"\njava version \"1.8.0_402\""),
            Some(8)
        );
    }

    use super::*;

    /// A fresh, empty scratch directory for one test.
    fn scratch(name: &str) -> Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!("regia_cli_test_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn touch(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, "")?;
        Ok(())
    }

    #[test]
    fn version_parsing_and_ordering() {
        assert_eq!(parse_version("8.14.3"), Some(vec![8, 14, 3]));
        assert_eq!(parse_version("28.0.13004108"), Some(vec![28, 0, 13004108]));
        assert_eq!(parse_version("8.14-rc-1"), None);
        assert_eq!(parse_version(""), None);
        assert!(parse_version("8.14") < parse_version("8.14.3"));
        assert!(parse_version("8.9") < parse_version("8.10.2"));
    }

    #[test]
    fn rust_targets_for_abis() {
        let abis: Vec<String> = ["arm64-v8a", "x86_64"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(abi_triple("armeabi-v7a"), Some("armv7-linux-androideabi"));
        assert_eq!(abi_triple("x86"), Some("i686-linux-android"));
        assert_eq!(abi_triple("mips"), None);
        let installed = "x86_64-unknown-linux-gnu\naarch64-linux-android\n";
        assert_eq!(missing_rust_targets(installed, &abis), ["x86_64-linux-android"]);
        let all = "aarch64-linux-android\nx86_64-linux-android\n";
        assert!(missing_rust_targets(all, &abis).is_empty());
    }

    #[test]
    fn java_major_version() {
        assert_eq!(
            parse_java_major("openjdk version \"17.0.20.1\" 2026-08-18\nOpenJDK Runtime"),
            Some(17)
        );
        assert_eq!(parse_java_major("java version \"1.8.0_402\""), Some(8));
        assert_eq!(parse_java_major("openjdk version \"21\" 2023-09-19"), Some(21));
        assert_eq!(parse_java_major("openjdk version \"25-ea\""), Some(25));
        assert_eq!(parse_java_major("no version here"), None);
    }

    /// CLI2 audit 8, beyond `audit_java_version_ignores_quoted_environment_notices` (from the
    /// fix-cli2-correctness lane): both notice lines at once, the runtime line after the version,
    /// and notices with no version line at all.
    #[test]
    fn java_version_ignores_picked_up_options() {
        let stderr = "Picked up JAVA_TOOL_OPTIONS: -Dapp.name=\"Regia\"\n\
                      Picked up _JAVA_OPTIONS: -Dx=\"9\"\n\
                      openjdk version \"17.0.2\" 2022-01-18\n\
                      OpenJDK Runtime Environment (build 17.0.2+8-86)\n";
        assert_eq!(parse_java_major(stderr), Some(17));
        let old = "Picked up _JAVA_OPTIONS: -Dfile.encoding=\"UTF-8\"\njava version \"1.8.0_402\"\n";
        assert_eq!(parse_java_major(old), Some(8));
        // Options alone carry no version.
        assert_eq!(
            parse_java_major("Picked up JAVA_TOOL_OPTIONS: -Dv=\"21\"\n"),
            None
        );
    }

    #[test]
    fn sdk_lookup_order() -> Result<()> {
        let dir = scratch("sdk")?;
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::create_dir_all(&a)?;
        std::fs::create_dir_all(&b)?;
        assert_eq!(find_sdk_from(Some(a.clone()), Some(b.clone()), None)?, a);
        assert_eq!(find_sdk_from(None, Some(b.clone()), None)?, b);
        let default = default_sdk_under(&dir);
        assert!(find_sdk_from(None, None, Some(dir.clone())).is_err());
        std::fs::create_dir_all(&default)?;
        assert_eq!(find_sdk_from(None, None, Some(dir.clone()))?, default);
        assert!(find_sdk_from(Some(dir.join("missing")), None, None).is_err());
        assert!(find_sdk_from(None, None, None).is_err());

        assert!(check_sdk_platform(&a, 35).is_err());
        std::fs::create_dir_all(a.join("platforms").join("android-35"))?;
        check_sdk_platform(&a, 35)?;
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn newest_ndk_is_picked() -> Result<()> {
        let sdk = scratch("ndk")?;
        assert_eq!(newest_ndk_in(&sdk), None);
        for v in ["26.1.10909125", "28.0.13004108", "27.2.12479018", "not-a-version"] {
            std::fs::create_dir_all(sdk.join("ndk").join(v))?;
        }
        let newest = sdk.join("ndk").join("28.0.13004108");
        assert_eq!(newest_ndk_in(&sdk), Some(newest.clone()));
        assert_eq!(ndk_version(&newest).as_deref(), Some("28.0.13004108"));
        std::fs::write(
            newest.join("source.properties"),
            "Pkg.Desc = Android NDK\nPkg.Revision = 28.0.1\n",
        )?;
        assert_eq!(ndk_version(&newest).as_deref(), Some("28.0.1"));
        assert_eq!(ndk_version(&sdk.join("ndk").join("not-a-version")), None);
        std::fs::remove_dir_all(&sdk)?;
        Ok(())
    }

    #[test]
    fn cached_gradle_prefers_pinned_then_newest_supported() -> Result<()> {
        let dists = scratch("gradle")?;
        let script = |ver: &str, flavour: &str| {
            dists
                .join(format!("gradle-{ver}-{flavour}"))
                .join("hash123")
                .join(format!("gradle-{ver}"))
                .join("bin")
                .join(gradle_script_name())
        };
        assert_eq!(cached_gradle_in(&dists), None);
        // Too old for the pinned Android Gradle plugin: never chosen.
        touch(&script("8.9", "bin"))?;
        touch(&script("8.11.1", "bin"))?;
        assert_eq!(cached_gradle_in(&dists), None);
        touch(&script("8.13", "all"))?;
        touch(&script("8.14", "bin"))?;
        assert_eq!(cached_gradle_in(&dists), Some(script("8.14", "bin")));
        touch(&script("8.14.2", "bin"))?;
        assert_eq!(cached_gradle_in(&dists), Some(script("8.14.2", "bin")));
        // Another major version is never picked implicitly.
        touch(&script("9.1.0", "bin"))?;
        assert_eq!(cached_gradle_in(&dists), Some(script("8.14.2", "bin")));
        touch(&script(GRADLE_VERSION, "bin"))?;
        assert_eq!(cached_gradle_in(&dists), Some(script(GRADLE_VERSION, "bin")));
        // A distribution directory without the launcher (interrupted download) is skipped.
        std::fs::create_dir_all(dists.join("gradle-9.9-bin").join("x").join("gradle-9.9"))?;
        assert_eq!(cached_gradle_in(&dists), Some(script(GRADLE_VERSION, "bin")));
        std::fs::remove_dir_all(&dists)?;
        Ok(())
    }
}
