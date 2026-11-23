// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask android-cfg`: find crate source trees with Android-only code, then compile the
//! packages whose Android code is outside the umbrella check.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result};

const ANDROID_CFG: &[u8] = b"target_os = \"android\"";
const CHECKED_PACKAGES: [&str; 2] = ["regia_cli", "regia_templates"];

/// Returns workspace crate names whose `src/` tree contains the Android cfg expression.
///
/// # Errors
/// Returns an IO error if a crate or source directory cannot be inspected.
pub fn android_cfg_crates(root: &Path) -> Result<Vec<String>> {
    let crates_dir = root.join("crates");
    let entries =
        std::fs::read_dir(&crates_dir).with_context(|| format!("reading {}", crates_dir.display()))?;
    let mut crates = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading entry in {}", crates_dir.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("reading file type for {}", entry.path().display()))?
            .is_dir()
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let source = entry.path().join("src");
        if !source.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        collect_files(&source, &mut files)?;
        let mut found_cfg = false;
        for file in &files {
            if file_contains_cfg(file).with_context(|| format!("scanning {}", file.display()))? {
                found_cfg = true;
                break;
            }
        }
        if found_cfg {
            crates.push(name);
        }
    }
    crates.sort();
    Ok(crates)
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(directory).with_context(|| format!("reading {}", directory.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("reading entry in {}", directory.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("reading file type for {}", path.display()))?;
        if file_type.is_dir() {
            collect_files(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn file_contains_cfg(path: &Path) -> std::io::Result<bool> {
    let contents = std::fs::read(path)?;
    Ok(contents
        .windows(ANDROID_CFG.len())
        .any(|window| window == ANDROID_CFG))
}

/// Scans Android cfg usage and checks the CLI and template packages for Android aarch64.
///
/// Crates with Android-specific source beyond those two checks are listed for follow-up and do
/// not affect the command's exit status.
///
/// # Errors
/// Returns an IO error when source files cannot be scanned or a child process cannot be started.
pub fn run(root: &Path) -> Result<ExitCode> {
    let crates = android_cfg_crates(root)?;
    println!("Android cfg source scan ({})", crates.len());
    for name in &crates {
        let checked = CHECKED_PACKAGES.contains(&name.as_str());
        println!(
            "  {name:<28} {}",
            if checked { "checked below" } else { "listed only" }
        );
    }

    let use_ndk = cargo_ndk_available(root);
    let mut failed = Vec::new();
    for package in CHECKED_PACKAGES {
        let mut command = Command::new("cargo");
        command.current_dir(root);
        if use_ndk {
            command.args(["ndk", "-t", "arm64-v8a", "check", "-p", package]);
        } else {
            command.args(["check", "--target", "aarch64-linux-android", "-p", package]);
        }
        println!("\nChecking {package} for aarch64-linux-android...");
        let status = command
            .status()
            .with_context(|| format!("running Android check for {package}"))?;
        if !status.success() {
            failed.push(package);
        }
    }
    if failed.is_empty() {
        println!(
            "android-cfg: checked {} and {}; other crates are listed only",
            CHECKED_PACKAGES[0], CHECKED_PACKAGES[1]
        );
        Ok(ExitCode::SUCCESS)
    } else {
        for package in &failed {
            eprintln!("android-cfg: Android check failed for {package}");
        }
        Ok(ExitCode::FAILURE)
    }
}

fn cargo_ndk_available(root: &Path) -> bool {
    Command::new("cargo")
        .current_dir(root)
        .args(["ndk", "--version"])
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use regia_test_utils::unique_temp_dir;

    use super::android_cfg_crates;

    #[test]
    fn scanner_lists_only_crates_with_android_cfg_text() {
        let root = unique_temp_dir("android-cfg-scanner");
        let android_src = root.join("crates/fixture_android/src");
        let plain_src = root.join("crates/fixture_plain/src");
        fs::create_dir_all(&android_src).expect("create Android fixture");
        fs::create_dir_all(&plain_src).expect("create plain fixture");
        fs::write(
            android_src.join("lib.rs"),
            "#[cfg(target_os = \"android\")]\nfn platform_only() {}\n",
        )
        .expect("write Android fixture");
        fs::write(plain_src.join("lib.rs"), "pub fn host_only() {}\n").expect("write plain fixture");

        let found = android_cfg_crates(&root).expect("scan fixture");

        assert_eq!(found, ["fixture_android"]);
        fs::remove_dir_all(root).expect("remove fixture");
    }
}
