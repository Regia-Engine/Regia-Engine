// SPDX-License-Identifier: MPL-2.0
//! The Linux AppImage format of `regia package` (`docs/specs/21-tooling-and-packaging.md` § 2 and
//! § 4, ADR-2631).
//!
//! `regia package --platform linux --format appimage` (or `formats = ["tar", "appimage"]` in
//! `[platforms.linux]`) turns the staged package into an AppDir and runs `appimagetool` on it:
//!
//! ```text
//! <Name>.AppDir/
//!   AppRun                 # 0755 sh: exec usr/bin/<bin> "$@"
//!   <slug>.desktop         # Type, Name, Exec=<bin>, Icon=<slug>, Categories=Game;
//!   <slug>.png             # the project icon (required)
//!   .DirIcon -> <slug>.png
//!   usr/bin/<bin>          # 0755, the game; content/, assets/, build_info.json, notices beside it
//! ```
//!
//! The game resolves its content next to its executable, so the staged package lands in `usr/bin`
//! unchanged. `appimagetool` is found through `REGIA_APPIMAGETOOL` or `PATH`; it is never
//! downloaded. Error codes: `appimagetool_missing`, `appimage_icon_missing`, `appimage_failed`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::process;

/// Environment variable that names the `appimagetool` executable.
pub const APPIMAGETOOL_ENV: &str = "REGIA_APPIMAGETOOL";

/// Where to get the tool, for the `appimagetool_missing` message.
const GET_APPIMAGETOOL: &str = "download appimagetool-<arch>.AppImage from \
https://github.com/AppImage/appimagetool/releases, mark it executable, and either put it on PATH as \
`appimagetool` or set REGIA_APPIMAGETOOL to its path (regia never downloads it)";

/// Finds `appimagetool`: the file `env` names, else `appimagetool` in a `path` directory.
#[must_use]
pub fn find_appimagetool(env: Option<OsString>, path: Option<OsString>) -> Option<PathBuf> {
    if let Some(named) = env.filter(|v| !v.is_empty()) {
        let named = PathBuf::from(named);
        return named.is_file().then_some(named);
    }
    std::env::split_paths(&path?)
        .map(|dir| dir.join("appimagetool"))
        .find(|candidate| candidate.is_file())
}

/// The `appimagetool` for this machine, or the `appimagetool_missing` error.
pub fn require_appimagetool() -> Result<PathBuf> {
    let env = regia_core::brand::env_var_os(APPIMAGETOOL_ENV);
    let named = env.as_ref().is_some_and(|v| !v.is_empty());
    find_appimagetool(env, regia_core::brand::env_var_os("PATH")).ok_or_else(|| {
        if named {
            anyhow::anyhow!("appimagetool_missing: {APPIMAGETOOL_ENV} names a file that does not exist; {GET_APPIMAGETOOL}")
        } else {
            anyhow::anyhow!("appimagetool_missing: `appimagetool` is not on PATH and {APPIMAGETOOL_ENV} is unset; {GET_APPIMAGETOOL}")
        }
    })
}

/// The CPU architecture name `appimagetool` takes in `ARCH`.
#[must_use]
pub fn appimage_arch() -> &'static str {
    std::env::consts::ARCH
}

fn plain(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// The `.desktop` entry of the AppDir: `Exec` is the binary name (the AppImage runtime puts it on
/// the launcher's PATH), `Icon` the icon file's stem at the AppDir root.
pub fn desktop_entry(name: &str, bin_name: &str, icon_stem: &str, version: &str) -> Result<String> {
    for (field, value) in [("[project] name", name), ("[project] version", version)] {
        if value.is_empty() || value.chars().any(char::is_control) {
            bail!(
                "{field} must be non-empty and free of control characters (it is written into the .desktop file)"
            );
        }
    }
    if !plain(bin_name) || !plain(icon_stem) {
        bail!(
            "the binary name `{bin_name}` and icon name `{icon_stem}` must use letters, digits, `_`, `-` and `.`"
        );
    }
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name}\n\
         Exec={bin_name}\n\
         Icon={icon_stem}\n\
         Terminal=false\n\
         Categories=Game;\n\
         X-AppImage-Version={version}\n"
    ))
}

/// Checks a desktop entry against what `appimagetool` needs: the `[Desktop Entry]` group, `Type=Application`,
/// a `Name`, `Exec` naming `bin_name` and `Icon` naming `icon_stem`.
pub fn validate_desktop_entry(text: &str, bin_name: &str, icon_stem: &str) -> Result<()> {
    let mut lines = text.lines();
    if lines.next() != Some("[Desktop Entry]") {
        bail!("the desktop entry must start with [Desktop Entry]");
    }
    let key = |wanted: &str| {
        text.lines()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| *k == wanted)
            .map(|(_, v)| v.trim())
    };
    if key("Type") != Some("Application") {
        bail!("Type must be Application");
    }
    if key("Name").is_none_or(str::is_empty) {
        bail!("Name is required");
    }
    if key("Exec").and_then(|v| v.split_whitespace().next()) != Some(bin_name) {
        bail!("Exec must start with `{bin_name}`");
    }
    if key("Icon") != Some(icon_stem) {
        bail!("Icon must be `{icon_stem}`");
    }
    if !key("Categories").is_some_and(|c| c.split(';').any(|c| c == "Game")) {
        bail!("Categories must include Game");
    }
    Ok(())
}

/// The `AppRun` script: runs the game from `usr/bin` with bundled libraries on the loader path.
#[must_use]
pub fn app_run(bin_name: &str) -> String {
    format!(
        "#!/bin/sh\n\
         HERE=\"$(dirname \"$(readlink -f \"$0\")\")\"\n\
         export LD_LIBRARY_PATH=\"$HERE/usr/bin${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}\"\n\
         exec \"$HERE/usr/bin/{bin_name}\" \"$@\"\n"
    )
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions of {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

/// Normalizes modes under `dir`: directories 0755, files 0755 when already executable, else 0644.
fn normalize_modes(dir: &Path) -> Result<()> {
    set_mode(dir, 0o755)?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            normalize_modes(&path)?;
        } else {
            set_mode(&path, if is_executable(&path) { 0o755 } else { 0o644 })?;
        }
    }
    Ok(())
}

/// Builds `appdir` from the staged package: the whole `staging` tree (minus the tarball's
/// `<slug>.desktop` and `icon.png`) under `usr/bin`, plus `AppRun`, the desktop entry and the icon at
/// the root. `icon` must be a PNG.
pub fn build_appdir(
    staging: &Path,
    appdir: &Path,
    slug: &str,
    bin_name: &str,
    name: &str,
    version: &str,
    icon: &Path,
) -> Result<()> {
    let icon_bytes = std::fs::read(icon).with_context(|| format!("reading the icon {}", icon.display()))?;
    if !icon_bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        bail!("appimage_icon_missing: {} is not a PNG file", icon.display());
    }
    let desktop = desktop_entry(name, bin_name, slug, version)?;
    validate_desktop_entry(&desktop, bin_name, slug)?;
    let binary = staging.join(bin_name);
    if !binary.is_file() {
        bail!(
            "appimage_failed: the staged binary {} is missing",
            binary.display()
        );
    }

    if appdir.exists() {
        std::fs::remove_dir_all(appdir).with_context(|| format!("removing stale {}", appdir.display()))?;
    }
    let bin_dir = appdir.join("usr").join("bin");
    super::package::copy_dir(staging, &bin_dir)
        .with_context(|| format!("copying the package into {}", bin_dir.display()))?;
    for tar_only in [format!("{slug}.desktop"), "icon.png".to_owned()] {
        let _ = std::fs::remove_file(bin_dir.join(tar_only));
    }

    std::fs::write(appdir.join("AppRun"), app_run(bin_name))?;
    std::fs::write(appdir.join(format!("{slug}.desktop")), desktop)?;
    let icon_name = format!("{slug}.png");
    std::fs::write(appdir.join(&icon_name), &icon_bytes)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&icon_name, appdir.join(".DirIcon")).context("creating .DirIcon")?;
    #[cfg(not(unix))]
    std::fs::write(appdir.join(".DirIcon"), &icon_bytes)?;

    normalize_modes(appdir)?;
    set_mode(&appdir.join("AppRun"), 0o755)?;
    set_mode(&bin_dir.join(bin_name), 0o755)?;
    Ok(())
}

/// Runs `tool` on `appdir`, producing `output`. `APPIMAGE_EXTRACT_AND_RUN=1` lets an
/// `appimagetool` that is itself an AppImage run without FUSE.
pub fn run_appimagetool(tool: &Path, appdir: &Path, output: &Path) -> Result<()> {
    if output.exists() {
        std::fs::remove_file(output).with_context(|| format!("removing stale {}", output.display()))?;
    }
    process::run(
        Command::new(tool)
            .env("ARCH", appimage_arch())
            .env("APPIMAGE_EXTRACT_AND_RUN", "1")
            .arg(appdir)
            .arg(output),
        &format!(
            "check the appimagetool at {} runs; {GET_APPIMAGETOOL}",
            tool.display()
        ),
    )
    .context("appimage_failed: appimagetool did not finish")?;
    if !output.is_file() {
        bail!(
            "appimage_failed: appimagetool succeeded but {} does not exist",
            output.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    fn staged(name: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir(name);
        let staging = dir.join("Game");
        std::fs::create_dir_all(staging.join("content")).expect("mkdir");
        std::fs::write(staging.join("mygame"), b"\x7fELFfake").expect("bin");
        std::fs::write(staging.join("content/base.fpak"), b"pak").expect("pak");
        std::fs::write(staging.join("build_info.json"), b"{}").expect("info");
        std::fs::write(staging.join("game.desktop"), b"tar only").expect("desktop");
        std::fs::write(staging.join("icon.png"), PNG).expect("icon");
        #[cfg(unix)]
        set_mode(&staging.join("mygame"), 0o755).expect("mode");
        dir
    }

    #[test]
    fn desktop_entry_has_the_required_keys_and_validates() {
        let text = desktop_entry("My Game", "mygame", "game", "1.2.3").expect("entry");
        for line in [
            "Type=Application",
            "Name=My Game",
            "Exec=mygame",
            "Icon=game",
            "Categories=Game;",
        ] {
            assert!(text.lines().any(|l| l == line), "{line} in\n{text}");
        }
        validate_desktop_entry(&text, "mygame", "game").expect("valid");
    }

    #[test]
    fn validation_catches_each_defect() {
        let good = desktop_entry("G", "mygame", "game", "1").expect("entry");
        assert!(
            validate_desktop_entry(&good, "other", "game").is_err(),
            "Exec names another binary"
        );
        assert!(
            validate_desktop_entry(&good, "mygame", "icon").is_err(),
            "Icon names another file"
        );
        assert!(
            validate_desktop_entry(&good.replace("Type=Application", "Type=Link"), "mygame", "game").is_err()
        );
        assert!(validate_desktop_entry(&good.replace("Name=G\n", ""), "mygame", "game").is_err());
        assert!(
            validate_desktop_entry(
                &good.replace("Categories=Game;", "Categories=Utility;"),
                "mygame",
                "game"
            )
            .is_err()
        );
        assert!(
            validate_desktop_entry(&good.replacen("[Desktop Entry]", "[Other]", 1), "mygame", "game")
                .is_err()
        );
    }

    #[test]
    fn desktop_entry_rejects_injection_and_odd_names() {
        assert!(desktop_entry("Evil\nExec=rm", "mygame", "game", "1").is_err());
        assert!(desktop_entry("G", "my game", "game", "1").is_err());
        assert!(desktop_entry("G", "mygame", "../icon", "1").is_err());
        assert!(desktop_entry("G", "mygame", "game", "1\n[X]").is_err());
    }

    #[test]
    fn app_run_execs_the_binary_from_usr_bin() {
        let script = app_run("mygame");
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("exec \"$HERE/usr/bin/mygame\" \"$@\""));
        assert!(script.contains("LD_LIBRARY_PATH"));
    }

    #[test]
    fn find_tool_prefers_the_env_var_and_never_guesses() {
        let dir = regia_test_utils::unique_temp_dir("appimagetool-find");
        let on_path = dir.join("bin");
        std::fs::create_dir_all(&on_path).expect("mkdir");
        std::fs::write(on_path.join("appimagetool"), b"x").expect("tool");
        let custom = dir.join("custom-tool");
        std::fs::write(&custom, b"x").expect("custom");
        let path = std::env::join_paths([&on_path]).expect("path");
        assert_eq!(
            find_appimagetool(None, Some(path.clone())),
            Some(on_path.join("appimagetool"))
        );
        assert_eq!(
            find_appimagetool(Some(custom.clone().into()), Some(path.clone())),
            Some(custom)
        );
        assert_eq!(
            find_appimagetool(Some(dir.join("nope").into()), Some(path)),
            None,
            "a wrong env var is an error, not a fallback"
        );
        assert_eq!(find_appimagetool(None, None), None);
        assert_eq!(
            find_appimagetool(None, Some(std::env::join_paths([dir.join("empty")]).expect("p"))),
            None
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn appdir_layout() {
        let dir = staged("appdir-layout");
        let appdir = dir.join("Game.AppDir");
        build_appdir(
            &dir.join("Game"),
            &appdir,
            "game",
            "mygame",
            "My Game",
            "1.0.0",
            &dir.join("Game/icon.png"),
        )
        .expect("appdir");
        for file in [
            "AppRun",
            "game.desktop",
            "game.png",
            ".DirIcon",
            "usr/bin/mygame",
            "usr/bin/content/base.fpak",
            "usr/bin/build_info.json",
        ] {
            assert!(appdir.join(file).exists(), "{file} in the AppDir");
        }
        assert!(
            !appdir.join("usr/bin/game.desktop").exists(),
            "the tarball's desktop file stays out"
        );
        assert!(!appdir.join("usr/bin/icon.png").exists());
        let desktop = std::fs::read_to_string(appdir.join("game.desktop")).expect("desktop");
        validate_desktop_entry(&desktop, "mygame", "game").expect("valid");
        assert!(
            appdir
                .join(
                    desktop
                        .lines()
                        .find_map(|l| l.strip_prefix("Icon="))
                        .map(|i| format!("{i}.png"))
                        .expect("icon key")
                )
                .is_file()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn appdir_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = staged("appdir-modes");
        // A restrictive umask must not leak into the image.
        set_mode(&dir.join("Game/content/base.fpak"), 0o600).expect("mode");
        let appdir = dir.join("Game.AppDir");
        build_appdir(
            &dir.join("Game"),
            &appdir,
            "game",
            "mygame",
            "My Game",
            "1.0.0",
            &dir.join("Game/icon.png"),
        )
        .expect("appdir");
        let mode = |p: &str| {
            std::fs::metadata(appdir.join(p))
                .expect("meta")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("AppRun"), 0o755);
        assert_eq!(mode("usr/bin/mygame"), 0o755);
        assert_eq!(mode("usr/bin/content/base.fpak"), 0o644);
        assert_eq!(mode("game.desktop"), 0o644);
        assert_eq!(mode("game.png"), 0o644);
        assert_eq!(mode(""), 0o755);
        assert_eq!(mode("usr/bin/content"), 0o755);
        assert_eq!(
            std::fs::read_link(appdir.join(".DirIcon")).expect("link"),
            Path::new("game.png")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn appdir_refuses_a_non_png_icon_and_a_missing_binary() {
        let dir = staged("appdir-bad");
        let appdir = dir.join("Game.AppDir");
        std::fs::write(dir.join("bad.png"), b"GIF89a").expect("bad");
        let err = build_appdir(
            &dir.join("Game"),
            &appdir,
            "game",
            "mygame",
            "G",
            "1",
            &dir.join("bad.png"),
        )
        .expect_err("not a png");
        assert!(err.to_string().starts_with("appimage_icon_missing:"), "{err}");
        let err = build_appdir(
            &dir.join("Game"),
            &appdir,
            "game",
            "absent",
            "G",
            "1",
            &dir.join("Game/icon.png"),
        )
        .expect_err("no binary");
        assert!(err.to_string().starts_with("appimage_failed:"), "{err}");
        assert!(!appdir.exists(), "nothing half-built is left");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn appimagetool_is_invoked_with_the_appdir_and_output() {
        let dir = regia_test_utils::unique_temp_dir("appimagetool-run");
        let tool = dir.join("fake-appimagetool");
        // Records its arguments and the environment it was given into the "image".
        // Written by a child process, so no sibling test's fork can hold it open for writing
        // when it runs (the ETXTBSY flake, ADR-5959).
        regia_test_utils::process::write_executable_script(
            &tool,
            "#!/bin/sh\nprintf '%s|%s|%s|%s' \"$ARCH\" \"$APPIMAGE_EXTRACT_AND_RUN\" \"$1\" \"$2\" > \"$2\"\n",
        )
        .expect("tool");
        let appdir = dir.join("G.AppDir");
        std::fs::create_dir_all(&appdir).expect("appdir");
        let output = dir.join("G-x.AppImage");
        std::fs::write(&output, b"stale").expect("stale");
        run_appimagetool(&tool, &appdir, &output).expect("run");
        let got = std::fs::read_to_string(&output).expect("image");
        assert_eq!(
            got,
            format!("{}|1|{}|{}", appimage_arch(), appdir.display(), output.display())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_tool_that_writes_nothing_fails_with_appimage_failed() {
        let dir = regia_test_utils::unique_temp_dir("appimagetool-silent");
        let tool = dir.join("silent");
        regia_test_utils::process::write_executable_script(&tool, "#!/bin/sh\nexit 0\n").expect("tool");
        let err = run_appimagetool(&tool, &dir, &dir.join("out.AppImage")).expect_err("no output");
        assert!(err.to_string().starts_with("appimage_failed:"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
