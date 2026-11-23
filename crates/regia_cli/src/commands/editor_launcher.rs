// SPDX-License-Identifier: MPL-2.0
//! `regia editor install-launcher [--uninstall] [--editor-bin FILE] [--dry-run] [--json]`: adds the
//! Regia Editor to this desktop's app launcher, per user, never with elevated rights (ADR-20000).
//!
//! - **Linux:** `regia.editor.desktop` in `$XDG_DATA_HOME/applications` (default
//!   `~/.local/share/applications`) and the icon in `$XDG_DATA_HOME/icons/hicolor` (PNG sizes and
//!   the scalable SVG); then `update-desktop-database` and `gtk-update-icon-cache` when they are
//!   installed. The entry's name equals the editor window's Wayland app_id and X11 class
//!   (`regia.editor`), so docks match the running window to it.
//! - **Windows:** `Regia Editor.lnk` in `%APPDATA%\Microsoft\Windows\Start Menu\Programs` (made
//!   with the shell's `WScript.Shell` object through PowerShell) and its icon in
//!   `%LOCALAPPDATA%\Regia\Editor`.
//! - **macOS:** `~/Applications/Regia Editor.app`, a bundle whose executable starts the editor
//!   binary, with the `.icns` icon. It is unsigned: the first launch needs right-click > Open.
//!
//! The icons are the editor's icon set (`crates/regia_editor/assets/icon`, drawn by
//! `cargo xtask editor-icons`), embedded in this binary from the byte-identical copy in
//! `crates/regia_cli/assets/editor-icon` (a crate compiles from its own folder, ADR-1485). The launcher starts the editor binary
//! `regia editor` would run (`--editor-bin`, `REGIA_EDITOR`, next to this `regia`, `PATH`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::Args;
use serde_json::json;

/// The desktop entry's file name: the editor window's app id plus `.desktop`.
pub const DESKTOP_FILE: &str = "regia.editor.desktop";

/// The editor window's Wayland app_id and X11 class (`regia_editor::app_icon::APP_ID`).
pub const APP_ID: &str = "regia.editor";

/// The icon theme name (`Icon=` of the desktop entry).
pub const ICON_NAME: &str = "regia-editor";

/// The launcher's display name on every platform.
pub const DISPLAY_NAME: &str = "Regia Editor";

/// The macOS bundle identifier (the maker brand's reverse-DNS prefix).
pub const BUNDLE_ID: &str = "com.piercingxx.regia.editor";

/// The oldest macOS the bundle declares (owner's macOS decision, 2026-11-19).
pub const MAC_MIN_SYSTEM: &str = "13.0";

/// The hicolor PNG sizes installed on Linux.
pub const HICOLOR_SIZES: [u32; 8] = [16, 24, 32, 48, 64, 128, 256, 512];

/// Error code: no editor binary to point the launcher at.
pub const CODE_NO_EDITOR: &str = "launcher_no_editor";
/// Error code: the per-user directory could not be found (`HOME`, `APPDATA`, `LOCALAPPDATA`).
pub const CODE_NO_HOME: &str = "launcher_no_home";
/// Error code: writing or removing a launcher file failed.
pub const CODE_WRITE_FAILED: &str = "launcher_write_failed";
/// Error code: the Windows shortcut could not be made.
pub const CODE_SHORTCUT_FAILED: &str = "launcher_shortcut_failed";
/// Error code: a path cannot be written into a launcher file (a control character, not UTF-8).
pub const CODE_BAD_PATH: &str = "launcher_bad_path";
/// Error code: `--uninstall` found a bundle at the launcher's place that this command did not make.
pub const CODE_NOT_OURS: &str = "launcher_not_ours";
/// Error code: this desktop has no launcher this command knows.
pub const CODE_UNSUPPORTED: &str = "launcher_unsupported_os";

const SVG: &[u8] = include_bytes!("../../assets/editor-icon/regia-editor.svg");
const ICO: &[u8] = include_bytes!("../../assets/editor-icon/regia-editor.ico");
const ICNS: &[u8] = include_bytes!("../../assets/editor-icon/regia-editor.icns");
const PNG_16: &[u8] = include_bytes!("../../assets/editor-icon/regia-editor-16.png");
const PNG_24: &[u8] = include_bytes!("../../assets/editor-icon/regia-editor-24.png");
const PNG_32: &[u8] = include_bytes!("../../assets/editor-icon/regia-editor-32.png");
const PNG_48: &[u8] = include_bytes!("../../assets/editor-icon/regia-editor-48.png");
const PNG_64: &[u8] = include_bytes!("../../assets/editor-icon/regia-editor-64.png");
const PNG_128: &[u8] = include_bytes!("../../assets/editor-icon/regia-editor-128.png");
const PNG_256: &[u8] = include_bytes!("../../assets/editor-icon/regia-editor-256.png");
const PNG_512: &[u8] = include_bytes!("../../assets/editor-icon/regia-editor-512.png");

/// The embedded PNG of one hicolor size.
#[must_use]
pub fn png_for(size: u32) -> Option<&'static [u8]> {
    Some(match size {
        16 => PNG_16,
        24 => PNG_24,
        32 => PNG_32,
        48 => PNG_48,
        64 => PNG_64,
        128 => PNG_128,
        256 => PNG_256,
        512 => PNG_512,
        _ => return None,
    })
}

/// Arguments of `regia editor install-launcher`.
#[derive(Debug, Clone, Args)]
pub struct InstallLauncherArgs {
    /// Remove the launcher entry and its icons instead.
    #[arg(long)]
    pub uninstall: bool,
    /// The editor binary the launcher starts (default: `REGIA_EDITOR`, then `regia-editor` next to
    /// this `regia`, then on `PATH`).
    #[arg(long, value_name = "FILE")]
    pub editor_bin: Option<PathBuf>,
    /// Print what would be written or removed, change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// One JSON object on stdout (`book/agents/editor-launcher.md`).
    #[arg(long)]
    pub json: bool,
}

/// The desktop a launcher is made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    /// A freedesktop.org desktop (Linux).
    Linux,
    /// Windows' Start Menu.
    Windows,
    /// macOS' Applications folder.
    Mac,
}

impl Desktop {
    /// This host's desktop, if it has one this command knows.
    #[must_use]
    pub fn host() -> Option<Self> {
        match std::env::consts::OS {
            "linux" => Some(Self::Linux),
            "windows" => Some(Self::Windows),
            "macos" => Some(Self::Mac),
            _ => None,
        }
    }

    /// `linux`, `windows` or `macos`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::Mac => "macos",
        }
    }
}

/// One file the launcher writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherFile {
    /// Where it goes.
    pub path: PathBuf,
    /// Its bytes.
    pub bytes: Vec<u8>,
    /// Unix permission bits (ignored on Windows).
    pub mode: u32,
}

/// A Windows shell shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    /// The `.lnk` file.
    pub link: PathBuf,
    /// The program it starts.
    pub target: PathBuf,
    /// Its working directory.
    pub working_dir: PathBuf,
    /// The icon file it shows.
    pub icon: PathBuf,
}

/// Everything one platform's launcher consists of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherPlan {
    /// The desktop it is for.
    pub desktop: Desktop,
    /// The files written on install and removed on uninstall.
    pub files: Vec<LauncherFile>,
    /// A directory removed whole on uninstall (the macOS bundle), after checking it is ours.
    pub bundle: Option<PathBuf>,
    /// The Windows shortcut.
    pub shortcut: Option<Shortcut>,
    /// Cache refresh commands run after a change when the program is installed (program, args).
    pub refresh: Vec<(String, Vec<String>)>,
}

impl LauncherPlan {
    /// The launcher's main path: the desktop entry, the shortcut or the bundle.
    #[must_use]
    pub fn entry(&self) -> Option<PathBuf> {
        if let Some(bundle) = &self.bundle {
            return Some(bundle.clone());
        }
        if let Some(shortcut) = &self.shortcut {
            return Some(shortcut.link.clone());
        }
        self.files.first().map(|f| f.path.clone())
    }

    /// Every path the launcher owns (files, shortcut, bundle).
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self.files.iter().map(|f| f.path.clone()).collect();
        out.extend(self.shortcut.iter().map(|s| s.link.clone()));
        out.extend(self.bundle.iter().cloned());
        out
    }
}

/// A launcher error with its stable code (`<code>: <message>`, the CLI's error style).
fn fail(code: &str, message: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("{code}: {message}")
}

/// `path` as UTF-8 text free of control characters (it is written into a launcher file).
fn text_path(path: &Path) -> Result<&str> {
    let text = path
        .to_str()
        .ok_or_else(|| fail(CODE_BAD_PATH, format!("{} is not valid UTF-8", path.display())))?;
    if text.chars().any(char::is_control) {
        return Err(fail(
            CODE_BAD_PATH,
            format!("{} has a control character", path.display()),
        ));
    }
    Ok(text)
}

/// One `Exec=` argument, quoted by the Desktop Entry Specification's rules: always in double
/// quotes; `"`, `` ` ``, `$` and `\` escaped with a backslash; `%` doubled. Then the string-value
/// escape doubles every backslash once more.
#[must_use]
pub fn exec_quote(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\")
}

/// A desktop entry string value: backslashes doubled.
fn desktop_string(value: &str) -> String {
    value.replace('\\', "\\\\")
}

/// The Linux desktop entry that starts `editor`. `%F` passes opened project folders or files.
///
/// # Errors
///
/// The editor path is not UTF-8 or has a control character (`launcher_bad_path`).
pub fn desktop_entry(editor: &Path) -> Result<String> {
    let exe = text_path(editor)?;
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.5\n\
         Name={DISPLAY_NAME}\n\
         GenericName=Game Engine Editor\n\
         Comment=Build and edit games with Regia Engine\n\
         Exec={} %F\n\
         TryExec={}\n\
         Icon={ICON_NAME}\n\
         Terminal=false\n\
         Categories=Development;Graphics;3DGraphics;\n\
         Keywords=game;engine;3d;level;editor;\n\
         StartupNotify=true\n\
         StartupWMClass={APP_ID}\n",
        exec_quote(exe),
        desktop_string(exe),
    ))
}

/// Text escaped for an XML element.
fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The macOS bundle's `Info.plist` (`version` is the engine version).
#[must_use]
pub fn info_plist(version: &str) -> String {
    let version = xml_text(version);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>{DISPLAY_NAME}</string>
	<key>CFBundleExecutable</key>
	<string>regia-editor</string>
	<key>CFBundleIconFile</key>
	<string>regia-editor.icns</string>
	<key>CFBundleIdentifier</key>
	<string>{BUNDLE_ID}</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>{DISPLAY_NAME}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>LSMinimumSystemVersion</key>
	<string>{MAC_MIN_SYSTEM}</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
"#
    )
}

/// `text` in POSIX shell single quotes.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The bundle's executable: a shell script that replaces itself with the editor (same process,
/// so Launch Services keeps it as the bundle's app and the Dock shows the bundle's icon).
///
/// # Errors
///
/// The editor path is not UTF-8 or has a control character (`launcher_bad_path`).
pub fn mac_launch_script(editor: &Path) -> Result<String> {
    let exe = text_path(editor)?;
    Ok(format!(
        "#!/bin/sh\n# Regia Editor launcher, written by `regia editor install-launcher` (ADR-20000).\nexec {} \"$@\"\n",
        shell_quote(exe)
    ))
}

/// The per-user directories a launcher goes to, read from the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserDirs {
    /// `HOME`.
    pub home: Option<PathBuf>,
    /// `XDG_DATA_HOME`.
    pub xdg_data_home: Option<PathBuf>,
    /// `APPDATA` (Windows' roaming profile).
    pub appdata: Option<PathBuf>,
    /// `LOCALAPPDATA`.
    pub local_appdata: Option<PathBuf>,
}

impl UserDirs {
    /// From an environment lookup (empty values count as unset).
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| env(key).filter(|v| !v.is_empty()).map(PathBuf::from);
        Self {
            home: get("HOME").or_else(|| get("USERPROFILE")),
            xdg_data_home: get("XDG_DATA_HOME").filter(|p| p.is_absolute()),
            appdata: get("APPDATA"),
            local_appdata: get("LOCALAPPDATA"),
        }
    }

    /// The process environment.
    #[must_use]
    pub fn current() -> Self {
        Self::from_env(|key| std::env::var(key).ok())
    }

    /// `$XDG_DATA_HOME`, else `~/.local/share`.
    fn data_home(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.xdg_data_home {
            return Ok(dir.clone());
        }
        self.home
            .as_ref()
            .map(|home| home.join(".local").join("share"))
            .ok_or_else(|| fail(CODE_NO_HOME, "neither XDG_DATA_HOME nor HOME is set"))
    }
}

/// The launcher's main path on `desktop` (what `regia doctor` looks for), if the directories
/// are known.
#[must_use]
pub fn entry_path(desktop: Desktop, dirs: &UserDirs) -> Option<PathBuf> {
    match desktop {
        Desktop::Linux => dirs
            .data_home()
            .ok()
            .map(|d| d.join("applications").join(DESKTOP_FILE)),
        Desktop::Windows => dirs
            .appdata
            .as_ref()
            .map(|a| start_menu(a).join(format!("{DISPLAY_NAME}.lnk"))),
        Desktop::Mac => dirs.home.as_deref().map(mac_bundle),
    }
}

fn start_menu(appdata: &Path) -> PathBuf {
    appdata
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
}

fn mac_bundle(home: &Path) -> PathBuf {
    home.join("Applications").join(format!("{DISPLAY_NAME}.app"))
}

/// What the launcher for `editor` on `desktop` consists of.
///
/// # Errors
///
/// A per-user directory is not set (`launcher_no_home`), or the editor path cannot be written
/// into a launcher file (`launcher_bad_path`).
pub fn plan(desktop: Desktop, dirs: &UserDirs, editor: &Path) -> Result<LauncherPlan> {
    match desktop {
        Desktop::Linux => {
            let data = dirs.data_home()?;
            let apps = data.join("applications");
            let hicolor = data.join("icons").join("hicolor");
            let mut files = vec![LauncherFile {
                path: apps.join(DESKTOP_FILE),
                bytes: desktop_entry(editor)?.into_bytes(),
                mode: 0o644,
            }];
            for size in HICOLOR_SIZES {
                let bytes = png_for(size).ok_or_else(|| anyhow!("no {size} px icon"))?;
                files.push(LauncherFile {
                    path: hicolor
                        .join(format!("{size}x{size}"))
                        .join("apps")
                        .join(format!("{ICON_NAME}.png")),
                    bytes: bytes.to_vec(),
                    mode: 0o644,
                });
            }
            files.push(LauncherFile {
                path: hicolor
                    .join("scalable")
                    .join("apps")
                    .join(format!("{ICON_NAME}.svg")),
                bytes: SVG.to_vec(),
                mode: 0o644,
            });
            let refresh = vec![
                (
                    "update-desktop-database".to_owned(),
                    vec![text_path(&apps)?.to_owned()],
                ),
                (
                    "gtk-update-icon-cache".to_owned(),
                    vec!["-f".to_owned(), "-t".to_owned(), text_path(&hicolor)?.to_owned()],
                ),
            ];
            Ok(LauncherPlan {
                desktop,
                files,
                bundle: None,
                shortcut: None,
                refresh,
            })
        }
        Desktop::Windows => {
            let appdata = dirs
                .appdata
                .as_ref()
                .ok_or_else(|| fail(CODE_NO_HOME, "APPDATA is not set"))?;
            let local = dirs
                .local_appdata
                .as_ref()
                .ok_or_else(|| fail(CODE_NO_HOME, "LOCALAPPDATA is not set"))?;
            let icon = local
                .join("Regia")
                .join("Editor")
                .join(format!("{ICON_NAME}.ico"));
            let working_dir = editor.parent().map(Path::to_path_buf).unwrap_or_default();
            text_path(editor)?;
            Ok(LauncherPlan {
                desktop,
                files: vec![LauncherFile {
                    path: icon.clone(),
                    bytes: ICO.to_vec(),
                    mode: 0o644,
                }],
                bundle: None,
                shortcut: Some(Shortcut {
                    link: start_menu(appdata).join(format!("{DISPLAY_NAME}.lnk")),
                    target: editor.to_path_buf(),
                    working_dir,
                    icon,
                }),
                refresh: Vec::new(),
            })
        }
        Desktop::Mac => {
            let home = dirs
                .home
                .as_ref()
                .ok_or_else(|| fail(CODE_NO_HOME, "HOME is not set"))?;
            let bundle = mac_bundle(home);
            let contents = bundle.join("Contents");
            let files = vec![
                LauncherFile {
                    path: contents.join("Info.plist"),
                    bytes: info_plist(env!("CARGO_PKG_VERSION")).into_bytes(),
                    mode: 0o644,
                },
                LauncherFile {
                    path: contents.join("PkgInfo"),
                    bytes: b"APPL????".to_vec(),
                    mode: 0o644,
                },
                LauncherFile {
                    path: contents.join("MacOS").join("regia-editor"),
                    bytes: mac_launch_script(editor)?.into_bytes(),
                    mode: 0o755,
                },
                LauncherFile {
                    path: contents.join("Resources").join("regia-editor.icns"),
                    bytes: ICNS.to_vec(),
                    mode: 0o644,
                },
            ];
            let lsregister = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
            Ok(LauncherPlan {
                desktop,
                files,
                bundle: Some(bundle.clone()),
                shortcut: None,
                refresh: vec![(
                    lsregister.to_owned(),
                    vec!["-f".to_owned(), text_path(&bundle)?.to_owned()],
                )],
            })
        }
    }
}

/// The PowerShell script that writes `shortcut` (the paths arrive in environment variables, so
/// no path is ever parsed as script text).
pub const SHORTCUT_SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
     $s = (New-Object -ComObject WScript.Shell).CreateShortcut($env:REGIA_LNK); \
     $s.TargetPath = $env:REGIA_LNK_TARGET; \
     $s.WorkingDirectory = $env:REGIA_LNK_WORKDIR; \
     $s.IconLocation = $env:REGIA_LNK_ICON + ',0'; \
     $s.Description = 'Regia Editor'; \
     $s.Save()";

/// The environment `SHORTCUT_SCRIPT` reads for `shortcut`.
#[must_use]
pub fn shortcut_env(shortcut: &Shortcut) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("REGIA_LNK", shortcut.link.clone()),
        ("REGIA_LNK_TARGET", shortcut.target.clone()),
        ("REGIA_LNK_WORKDIR", shortcut.working_dir.clone()),
        ("REGIA_LNK_ICON", shortcut.icon.clone()),
    ]
}

fn make_shortcut(shortcut: &Shortcut) -> Result<()> {
    if let Some(dir) = shortcut.link.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| fail(CODE_WRITE_FAILED, format!("creating {}: {e}", dir.display())))?;
    }
    let mut command = std::process::Command::new("powershell.exe");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        SHORTCUT_SCRIPT,
    ]);
    for (key, value) in shortcut_env(shortcut) {
        command.env(key, value);
    }
    let out = command
        .output()
        .map_err(|e| fail(CODE_SHORTCUT_FAILED, format!("cannot run powershell.exe: {e}")))?;
    if !out.status.success() || !shortcut.link.is_file() {
        return Err(fail(
            CODE_SHORTCUT_FAILED,
            format!(
                "powershell could not write {}: {}",
                shortcut.link.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// Writes `bytes` to `path` through a temporary file in the same directory.
fn write_atomic(file: &LauncherFile) -> Result<()> {
    let path = &file.path;
    let dir = path
        .parent()
        .ok_or_else(|| fail(CODE_WRITE_FAILED, format!("{} has no parent", path.display())))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| fail(CODE_WRITE_FAILED, format!("creating {}: {e}", dir.display())))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("launcher");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &file.bytes)
        .map_err(|e| fail(CODE_WRITE_FAILED, format!("writing {}: {e}", tmp.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(file.mode))
            .map_err(|e| fail(CODE_WRITE_FAILED, format!("chmod {}: {e}", tmp.display())))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        fail(CODE_WRITE_FAILED, format!("writing {}: {e}", path.display()))
    })
}

/// Removes `path` if it is there.
fn remove_file(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(fail(
            CODE_WRITE_FAILED,
            format!("removing {}: {e}", path.display()),
        )),
    }
}

/// Runs the cache refresh commands that are installed; returns the ones that ran successfully.
fn refresh(plan: &LauncherPlan) -> Vec<String> {
    let mut ran = Vec::new();
    for (program, args) in &plan.refresh {
        let ok = std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            ran.push(program.clone());
        }
    }
    ran
}

/// Writes the launcher of `plan`; returns the paths written.
///
/// # Errors
///
/// A file or the shortcut cannot be written.
pub fn install(plan: &LauncherPlan) -> Result<Vec<PathBuf>> {
    // A bundle from an older install: replace it whole, so no stale file stays inside.
    if let Some(bundle) = &plan.bundle
        && bundle.exists()
    {
        remove_bundle(bundle)?;
    }
    let mut written = Vec::new();
    for file in &plan.files {
        write_atomic(file)?;
        written.push(file.path.clone());
    }
    if let Some(shortcut) = &plan.shortcut {
        make_shortcut(shortcut)?;
        written.push(shortcut.link.clone());
    }
    Ok(written)
}

/// Removes a bundle this command made (its `Info.plist` names [`BUNDLE_ID`]).
fn remove_bundle(bundle: &Path) -> Result<()> {
    let plist = std::fs::read_to_string(bundle.join("Contents").join("Info.plist")).unwrap_or_default();
    if !plist.contains(&format!("<string>{BUNDLE_ID}</string>")) {
        return Err(fail(
            CODE_NOT_OURS,
            format!(
                "{} is not a bundle `regia editor install-launcher` made (no {BUNDLE_ID}); remove it yourself",
                bundle.display()
            ),
        ));
    }
    std::fs::remove_dir_all(bundle)
        .map_err(|e| fail(CODE_WRITE_FAILED, format!("removing {}: {e}", bundle.display())))
}

/// Removes the launcher of `plan`; returns the paths removed (missing ones are skipped).
///
/// # Errors
///
/// A file cannot be removed, or the bundle at the launcher's place is not ours.
pub fn uninstall(plan: &LauncherPlan) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    if let Some(bundle) = &plan.bundle {
        if bundle.exists() {
            remove_bundle(bundle)?;
            removed.push(bundle.clone());
        }
        return Ok(removed);
    }
    for path in plan
        .files
        .iter()
        .map(|f| &f.path)
        .chain(plan.shortcut.iter().map(|s| &s.link))
    {
        if remove_file(path)? {
            removed.push(path.clone());
        }
    }
    // The Windows icon's own folder, when it is empty now.
    if plan.desktop == Desktop::Windows
        && let Some(dir) = plan.files.first().and_then(|f| f.path.parent())
    {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(removed)
}

/// The first `name` file on `path_var` (a `PATH` value).
fn find_on_path(name: &Path, path_var: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let var = path_var?;
    std::env::split_paths(&var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The editor binary as an absolute path (`regia editor`'s lookup, then `PATH`).
///
/// # Errors
///
/// No editor binary is found (`launcher_no_editor`).
pub fn resolve_editor(explicit: Option<&Path>) -> Result<PathBuf> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let candidate = super::editor::resolve_editor_bin(
        explicit,
        regia_core::brand::env_var("REGIA_EDITOR").ok().map(PathBuf::from),
        sibling.as_deref(),
    );
    let found = if candidate.components().count() == 1 && !candidate.is_file() {
        find_on_path(&candidate, std::env::var_os("PATH"))
    } else {
        Some(candidate.clone())
    };
    let path = found.filter(|p| p.is_file()).ok_or_else(|| {
        fail(
            CODE_NO_EDITOR,
            format!(
                "no editor binary at {} (build it with `cargo build --release -p regia_editor`, or pass --editor-bin / set REGIA_EDITOR)",
                candidate.display()
            ),
        )
    })?;
    let absolute = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    // Without Windows' `\\?\` prefix, which shortcuts and launchers do not take.
    Ok(PathBuf::from(regia_asset_pipeline::generators::plain_path(
        &absolute,
    )))
}

/// Runs `regia editor install-launcher`.
///
/// # Errors
///
/// See the `launcher_*` codes in `book/agents/editor-launcher.md`.
pub fn run(args: &InstallLauncherArgs) -> Result<()> {
    let result = run_inner(args);
    match (&result, args.json) {
        (Ok(value), true) => println!("{value}"),
        (Ok(value), false) => print_text(value),
        (Err(err), true) => {
            let text = err.to_string();
            let code = text
                .split(':')
                .next()
                .unwrap_or("launcher_failed")
                .trim()
                .to_owned();
            println!(
                "{}",
                json!({ "ok": false, "error": { "code": code, "message": text } })
            );
        }
        (Err(_), false) => {}
    }
    result.map(|_| ())
}

fn print_text(value: &serde_json::Value) {
    let action = value["action"].as_str().unwrap_or("install");
    let dry = value["dry_run"].as_bool().unwrap_or(false);
    let verb = match (action, dry) {
        ("install", false) => "wrote",
        ("install", true) => "would write",
        (_, false) => "removed",
        (_, true) => "would remove",
    };
    for path in value["paths"].as_array().into_iter().flatten() {
        println!("{verb} {}", path.as_str().unwrap_or_default());
    }
    if let Some(note) = value["note"].as_str() {
        println!("{note}");
    }
}

fn run_inner(args: &InstallLauncherArgs) -> Result<serde_json::Value> {
    let desktop = Desktop::host().ok_or_else(|| {
        fail(
            CODE_UNSUPPORTED,
            format!("no app launcher on {}", std::env::consts::OS),
        )
    })?;
    let dirs = UserDirs::current();
    let action = if args.uninstall { "uninstall" } else { "install" };
    // Uninstall needs no editor binary: the plan's paths do not depend on it.
    let editor = if args.uninstall {
        args.editor_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from(super::editor::editor_file_name()))
    } else {
        resolve_editor(args.editor_bin.as_deref())?
    };
    let plan = plan(desktop, &dirs, &editor)?;
    let paths = match (args.dry_run, args.uninstall) {
        (true, _) => plan.paths(),
        (false, false) => install(&plan)?,
        (false, true) => uninstall(&plan)?,
    };
    let refreshed = if args.dry_run { Vec::new() } else { refresh(&plan) };
    let note = match (desktop, args.uninstall) {
        (Desktop::Mac, false) => Some(
            "the app is unsigned: the first time, right-click Regia Editor in Applications and choose Open",
        ),
        (Desktop::Linux, false) => {
            Some("Regia Editor is in your app launcher (a dock may need a re-login to show the icon)")
        }
        (Desktop::Windows, false) => Some("Regia Editor is in the Start Menu"),
        _ => None,
    };
    Ok(json!({
        "ok": true,
        "action": action,
        "platform": desktop.name(),
        "dry_run": args.dry_run,
        "editor": if args.uninstall { serde_json::Value::Null } else { json!(editor.display().to_string()) },
        "entry": plan.entry().map(|p| p.display().to_string()),
        "paths": paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "refreshed": refreshed,
        "note": note,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(root: &Path) -> UserDirs {
        UserDirs {
            home: Some(root.join("home")),
            xdg_data_home: None,
            appdata: Some(root.join("AppData").join("Roaming")),
            local_appdata: Some(root.join("AppData").join("Local")),
        }
    }

    /// The `key=value` lines of a desktop entry's main group.
    fn entry_keys(text: &str) -> std::collections::BTreeMap<&str, &str> {
        text.lines().filter_map(|l| l.split_once('=')).collect()
    }

    #[test]
    fn desktop_entry_has_the_fields_docks_need() {
        let text = desktop_entry(Path::new("/opt/regia/bin/regia-editor")).expect("entry");
        assert!(text.starts_with("[Desktop Entry]\n"));
        let keys = entry_keys(&text);
        assert_eq!(keys["Type"], "Application");
        assert_eq!(keys["Name"], "Regia Editor");
        assert_eq!(keys["Exec"], r#""/opt/regia/bin/regia-editor" %F"#);
        assert_eq!(keys["TryExec"], "/opt/regia/bin/regia-editor");
        assert_eq!(keys["Icon"], ICON_NAME);
        assert_eq!(keys["Terminal"], "false");
        assert_eq!(keys["Categories"], "Development;Graphics;3DGraphics;");
        assert_eq!(keys["StartupWMClass"], APP_ID);
        assert!(keys.contains_key("Comment"));
        // The file name is the window's app id: what Wayland docks match.
        assert_eq!(DESKTOP_FILE, format!("{APP_ID}.desktop"));
    }

    #[test]
    fn exec_paths_are_quoted_and_escaped() {
        assert_eq!(exec_quote("/a b/c"), r#""/a b/c""#);
        assert_eq!(exec_quote("/x/100%"), r#""/x/100%%""#);
        assert_eq!(exec_quote("/x/$HOME"), r#""/x/\\$HOME""#);
        assert_eq!(exec_quote(r#"/x/"q""#), r#""/x/\\"q\\"""#);
        assert_eq!(exec_quote(r"/x\y"), r#""/x\\\\y""#);
        assert!(desktop_entry(Path::new("/bad\npath")).is_err());
    }

    #[test]
    fn info_plist_names_the_bundle() {
        let text = info_plist("1.2.3");
        for (key, value) in [
            ("CFBundleName", "Regia Editor"),
            ("CFBundleExecutable", "regia-editor"),
            ("CFBundleIconFile", "regia-editor.icns"),
            ("CFBundleIdentifier", BUNDLE_ID),
            ("CFBundlePackageType", "APPL"),
            ("CFBundleShortVersionString", "1.2.3"),
            ("LSMinimumSystemVersion", "13.0"),
        ] {
            let pair = format!("<key>{key}</key>\n\t<string>{value}</string>");
            assert!(text.contains(&pair), "{key} = {value} in\n{text}");
        }
        assert!(text.contains("<key>NSHighResolutionCapable</key>\n\t<true/>"));
        assert!(
            BUNDLE_ID.starts_with("com.piercingxx."),
            "the maker brand, no invented company"
        );
        assert!(info_plist("<1&2>").contains("&lt;1&amp;2&gt;"));
    }

    #[test]
    fn mac_script_quotes_the_editor_path() {
        let script = mac_launch_script(Path::new("/Users/o'brien/regia-editor")).expect("script");
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(
            script.ends_with("exec '/Users/o'\\''brien/regia-editor' \"$@\"\n"),
            "{script}"
        );
    }

    #[test]
    fn linux_plan_puts_files_under_the_data_home() {
        let root = Path::new("/r");
        let plan = plan(Desktop::Linux, &dirs(root), Path::new("/e/regia-editor")).expect("plan");
        let data = root.join("home/.local/share");
        assert_eq!(plan.entry(), Some(data.join("applications").join(DESKTOP_FILE)));
        let paths = plan.paths();
        assert_eq!(paths.len(), 1 + HICOLOR_SIZES.len() + 1);
        assert!(paths.contains(&data.join("icons/hicolor/48x48/apps/regia-editor.png")));
        assert!(paths.contains(&data.join("icons/hicolor/512x512/apps/regia-editor.png")));
        assert!(paths.contains(&data.join("icons/hicolor/scalable/apps/regia-editor.svg")));
        // XDG_DATA_HOME wins over HOME.
        let xdg = UserDirs {
            xdg_data_home: Some(PathBuf::from("/xdg")),
            ..dirs(root)
        };
        let plan = super::plan(Desktop::Linux, &xdg, Path::new("/e/regia-editor")).expect("plan");
        assert_eq!(
            plan.entry(),
            Some(PathBuf::from("/xdg/applications").join(DESKTOP_FILE))
        );
        assert!(super::plan(Desktop::Linux, &UserDirs::default(), Path::new("/e")).is_err());
    }

    #[test]
    fn relative_xdg_data_home_is_ignored() {
        let dirs = UserDirs::from_env(|k| match k {
            "HOME" => Some("/h".to_owned()),
            "XDG_DATA_HOME" => Some("relative".to_owned()),
            _ => None,
        });
        assert_eq!(
            dirs.xdg_data_home, None,
            "the spec says relative paths are invalid"
        );
        assert_eq!(
            entry_path(Desktop::Linux, &dirs),
            Some(PathBuf::from("/h/.local/share/applications").join(DESKTOP_FILE))
        );
    }

    #[test]
    fn windows_plan_is_a_start_menu_shortcut() {
        let root = Path::new("/r");
        let plan = plan(Desktop::Windows, &dirs(root), Path::new("/e/regia-editor.exe")).expect("plan");
        let shortcut = plan.shortcut.clone().expect("shortcut");
        assert_eq!(
            shortcut.link,
            root.join("AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Regia Editor.lnk")
        );
        assert_eq!(shortcut.target, Path::new("/e/regia-editor.exe"));
        assert_eq!(shortcut.working_dir, Path::new("/e"));
        assert_eq!(
            shortcut.icon,
            root.join("AppData/Local/Regia/Editor/regia-editor.ico")
        );
        assert_eq!(plan.files[0].bytes, ICO);
        assert_eq!(plan.entry(), Some(shortcut.link.clone()));
        let env: Vec<&str> = shortcut_env(&shortcut).iter().map(|(k, _)| *k).collect();
        for key in env {
            assert!(SHORTCUT_SCRIPT.contains(&format!("$env:{key}")), "{key}");
        }
        assert_eq!(entry_path(Desktop::Windows, &dirs(root)), Some(shortcut.link));
    }

    #[test]
    fn mac_plan_is_an_app_bundle() {
        let root = Path::new("/r");
        let plan = plan(Desktop::Mac, &dirs(root), Path::new("/e/regia-editor")).expect("plan");
        let bundle = root.join("home/Applications/Regia Editor.app");
        assert_eq!(plan.bundle.as_ref(), Some(&bundle));
        let names: Vec<PathBuf> = plan
            .files
            .iter()
            .map(|f| f.path.strip_prefix(&bundle).expect("inside").to_path_buf())
            .collect();
        assert_eq!(
            names,
            [
                PathBuf::from("Contents/Info.plist"),
                PathBuf::from("Contents/PkgInfo"),
                PathBuf::from("Contents/MacOS/regia-editor"),
                PathBuf::from("Contents/Resources/regia-editor.icns"),
            ]
        );
        assert_eq!(plan.files[2].mode, 0o755, "the launcher script is executable");
        assert_eq!(entry_path(Desktop::Mac, &dirs(root)), Some(bundle));
    }

    #[test]
    fn install_and_uninstall_round_trip_in_a_temp_home() {
        let root = regia_test_utils::unique_temp_dir("cli-editor-launcher");
        std::fs::create_dir_all(&root).expect("root");
        let dirs = dirs(&root);
        let editor = root.join("regia-editor");
        for desktop in [Desktop::Linux, Desktop::Mac] {
            let plan = plan(desktop, &dirs, &editor).expect("plan");
            let written = install(&plan).expect("install");
            assert_eq!(written.len(), plan.files.len());
            for file in &plan.files {
                assert_eq!(std::fs::read(&file.path).expect("written"), file.bytes);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = std::fs::metadata(&file.path).expect("meta").permissions().mode();
                    assert_eq!(mode & 0o777, file.mode, "{}", file.path.display());
                }
            }
            // A second install replaces the first cleanly.
            install(&plan).expect("reinstall");
            let removed = uninstall(&plan).expect("uninstall");
            assert!(!removed.is_empty());
            for file in &plan.files {
                assert!(!file.path.exists(), "{} removed", file.path.display());
            }
            assert!(
                uninstall(&plan).expect("again").is_empty(),
                "uninstall twice is a no-op"
            );
        }
        // A foreign bundle at the launcher's place is left alone.
        let plan = plan(Desktop::Mac, &dirs, &editor).expect("plan");
        let bundle = plan.bundle.clone().expect("bundle");
        std::fs::create_dir_all(bundle.join("Contents")).expect("foreign");
        std::fs::write(bundle.join("Contents/Info.plist"), "<string>org.other</string>").expect("plist");
        let err = uninstall(&plan).expect_err("not ours").to_string();
        assert!(err.starts_with(CODE_NOT_OURS), "{err}");
        assert!(bundle.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn embedded_icons_have_their_sizes() {
        for size in HICOLOR_SIZES {
            let png = png_for(size).expect("embedded");
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            let width = u32::from_be_bytes(png[16..20].try_into().expect("ihdr"));
            let height = u32::from_be_bytes(png[20..24].try_into().expect("ihdr"));
            assert_eq!((width, height), (size, size));
        }
        assert!(png_for(20).is_none());
    }

    /// ICO: a 16..256 directory, the 256 entry PNG-compressed, smaller ones DIBs.
    #[test]
    fn ico_has_sizes_16_to_256() {
        let ico = ICO;
        let le16 = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]);
        let le32 = |at: usize| u32::from_le_bytes(ico[at..at + 4].try_into().expect("4"));
        assert_eq!((le16(0), le16(2)), (0, 1), "ICONDIR of an icon");
        let count = usize::from(le16(4));
        let mut sides = Vec::new();
        for i in 0..count {
            let entry = 6 + 16 * i;
            let side = if ico[entry] == 0 {
                256
            } else {
                u32::from(ico[entry])
            };
            let (len, at) = (le32(entry + 8) as usize, le32(entry + 12) as usize);
            assert!(at + len <= ico.len(), "entry {i} inside the file");
            let png = ico[at..at + 8] == *b"\x89PNG\r\n\x1a\n";
            assert_eq!(png, side == 256, "only 256 is PNG-compressed");
            sides.push(side);
        }
        assert_eq!(sides, [16, 24, 32, 48, 64, 128, 256]);
    }

    /// ICNS: every entry's type and PNG side, and the lengths add up.
    #[test]
    fn icns_has_the_mac_icon_family() {
        let icns = ICNS;
        let be32 = |at: usize| u32::from_be_bytes(icns[at..at + 4].try_into().expect("4")) as usize;
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(be32(4), icns.len());
        let mut at = 8;
        let mut found = Vec::new();
        while at < icns.len() {
            let kind = String::from_utf8_lossy(&icns[at..at + 4]).into_owned();
            let len = be32(at + 4);
            let data = &icns[at + 8..at + len];
            assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n", "{kind} is a PNG");
            let side = u32::from_be_bytes(data[16..20].try_into().expect("ihdr"));
            found.push((kind, side));
            at += len;
        }
        assert_eq!(at, icns.len());
        let want: Vec<(String, u32)> = [
            ("icp4", 16),
            ("ic11", 32),
            ("icp5", 32),
            ("ic12", 64),
            ("ic07", 128),
            ("ic13", 256),
            ("ic08", 256),
            ("ic14", 512),
            ("ic09", 512),
            ("ic10", 1024),
        ]
        .iter()
        .map(|(k, s)| ((*k).to_owned(), *s))
        .collect();
        assert_eq!(found, want);
    }

    /// The CLI's copy of the icon set equals the editor's, file for file, when the editor's folder
    /// is there (a packaged `regia_cli` has only its own; `cargo xtask editor-icons` writes both).
    #[test]
    fn cli_icon_copy_matches_the_editor_icon_set() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let editor = manifest.join("../regia_editor/assets/icon");
        if !editor.is_dir() {
            return;
        }
        let copy = manifest.join("assets/editor-icon");
        let mut names: Vec<String> = std::fs::read_dir(&copy)
            .expect("the CLI's icon copy")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with("regia-editor"))
            .collect();
        names.sort();
        assert_eq!(names.len(), HICOLOR_SIZES.len() + 3, "{names:?}");
        for name in &names {
            let ours = std::fs::read(copy.join(name)).expect("copy");
            let theirs = std::fs::read(editor.join(name)).expect("editor file");
            assert!(
                ours == theirs,
                "{name} differs from the editor's: run `cargo xtask editor-icons`"
            );
        }
    }
}
