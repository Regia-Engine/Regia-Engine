# Editor launcher (agents)

`regia editor install-launcher` adds the Regia Editor to the host desktop's app launcher for the
current user; `--uninstall` removes it (ADR-20000). The user guide is
[The editor in your app launcher](../user-guide/editor-launcher.md). It is a CLI command only:
the editor has no menu item for it, and no agent protocol method, because it changes the user's
desktop, not a project.

```console
$ regia editor install-launcher --dry-run --json      # what it would write
$ regia editor install-launcher --json                # write it
$ regia editor install-launcher --uninstall --json    # remove it
$ regia editor install-launcher --editor-bin target/release/regia-editor
$ regia doctor --json                                 # editor-launcher: ok or editor_launcher_missing
```

## The JSON object

`--json` prints one object on stdout.

```json
{
  "ok": true,
  "action": "install",
  "platform": "linux",
  "dry_run": false,
  "editor": "/home/dev/regia/target/release/regia-editor",
  "entry": "/home/dev/.local/share/applications/regia.editor.desktop",
  "paths": [
    "/home/dev/.local/share/applications/regia.editor.desktop",
    "/home/dev/.local/share/icons/hicolor/16x16/apps/regia-editor.png",
    "...",
    "/home/dev/.local/share/icons/hicolor/scalable/apps/regia-editor.svg"
  ],
  "refreshed": ["update-desktop-database", "gtk-update-icon-cache"],
  "note": "Regia Editor is in your app launcher (a dock may need a re-login to show the icon)"
}
```

| Field | Meaning |
|---|---|
| `action` | `install` or `uninstall`. |
| `platform` | `linux`, `windows` or `macos`. |
| `editor` | The absolute editor binary the launcher starts; `null` on uninstall. |
| `entry` | The launcher's main path: the `.desktop` file, the `.lnk` shortcut or the `.app` bundle. |
| `paths` | The paths written (install), removed (uninstall; missing ones are skipped), or that would be (`--dry-run`). |
| `refreshed` | The cache commands that ran and succeeded (`update-desktop-database`, `gtk-update-icon-cache`, macOS `lsregister`). A missing tool is skipped. |
| `note` | A line for the user: the macOS first-launch step, the dock re-login hint. |

On an error the command exits 1 and, with `--json`, prints
`{"ok": false, "error": {"code": "<code>", "message": "<code>: ..."}}`. Without `--json` the error
line starts with the code.

## Error codes

| Code | Meaning |
|---|---|
| `launcher_no_editor` | No editor binary at `--editor-bin`, `REGIA_EDITOR`, next to `regia` or on `PATH`. Build it, or pass `--editor-bin`. |
| `launcher_no_home` | The per-user directory is unknown: `HOME` (Linux, macOS; or `XDG_DATA_HOME`), `APPDATA` and `LOCALAPPDATA` (Windows). |
| `launcher_bad_path` | The editor path is not UTF-8 or holds a control character, so it cannot be written into the launcher. |
| `launcher_write_failed` | A file or folder could not be written or removed (permissions, a full disk). |
| `launcher_shortcut_failed` | Windows: PowerShell could not save the `.lnk` (its error text is in the message). |
| `launcher_not_ours` | macOS: `~/Applications/Regia Editor.app` exists but has no `com.piercingxx.regia.editor` bundle id; the command leaves it alone. |
| `launcher_unsupported_os` | The host is not Linux, Windows or macOS. |

`regia doctor` reports the launcher in the host's section as `editor-launcher`: `ok` with its path,
or `info` with the code `editor_launcher_missing` (never a warning or a failure).

## What it writes

- **Linux:** `regia.editor.desktop` (`Exec="<editor>" %F`, `Icon=regia-editor`,
  `StartupWMClass=regia.editor`, `Categories=Development;Graphics;3DGraphics;`) in
  `$XDG_DATA_HOME/applications` (default `~/.local/share/applications`), and the hicolor icons
  (8 PNG sizes and the scalable SVG) under `$XDG_DATA_HOME/icons/hicolor`. The editor's window
  app id is `regia.editor`, equal to the file name, so docks match the window.
- **Windows:** `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Regia Editor.lnk` (target, working
  folder, icon) and `%LOCALAPPDATA%\Regia\Editor\regia-editor.ico`.
- **macOS:** `~/Applications/Regia Editor.app` with `Contents/Info.plist` (`CFBundleIdentifier`
  `com.piercingxx.regia.editor`, `LSMinimumSystemVersion` 13.0, `NSHighResolutionCapable`),
  `Contents/MacOS/regia-editor` (a script that `exec`s the editor binary),
  `Contents/Resources/regia-editor.icns` and `Contents/PkgInfo`. Unsigned.

Installing again overwrites the files (on macOS it replaces the bundle whole). Uninstalling twice
is not an error.

## The icon set

`crates/regia_editor/assets/icon/regia-editor.svg` is the only source. `cargo xtask editor-icons`
draws `regia-editor-<N>.png` (16 to 1024), `regia-editor.ico` (16 to 256; 256 PNG-compressed) and
`regia-editor.icns` (16 to 1024, the tile on 824/1024 of the canvas like other macOS icons) beside
it; `--check` exits 1 when a committed file differs from a fresh draw. The editor embeds the 128
and 256 PNGs for its window and taskbar icon (`regia_editor::app_icon`), its Windows build embeds
the `.ico` into `regia-editor.exe`, and `regia` embeds the set for this command.

## Verify

```console
cargo test -p regia_cli --bin regia editor          # entry, Info.plist, ICO/ICNS sizes, temp-HOME install and uninstall
cargo test -p regia_cli --bin regia doctor          # editor_launcher_missing / ok
cargo test -p regia_editor --lib app_icon           # window icon decode, app id, opened-path resolution
cargo test -p xtask --lib editor_icons              # ICO/ICNS writers, the drawn tile
cargo xtask editor-icons --check                    # committed icons match the SVG
```
