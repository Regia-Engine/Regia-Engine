# The editor in your app launcher

The Regia Editor has an app icon: the underlined-XX mark on a black rounded tile. Its window and
taskbar button show it, and one command adds the editor to your desktop's app launcher, so you can
start it like any other app (ADR-20000).

```console
$ regia editor install-launcher              # add the Regia Editor to this desktop's launcher
$ regia editor install-launcher --dry-run    # list what it would write, change nothing
$ regia editor install-launcher --uninstall  # remove it again
```

It installs for your user only and never asks for administrator rights. The launcher starts the
editor binary that `regia editor` would run: `--editor-bin FILE`, else `REGIA_EDITOR`, else
`regia-editor` next to `regia`, else the first one on `PATH`. Build a release editor first
(`cargo build --release -p regia_editor`) and point the launcher at it; run the command again
after you move the binary. `regia doctor` lists the launcher (`editor_launcher_missing` when there
is none).

## Linux

The command writes:

| File | What it is |
|---|---|
| `~/.local/share/applications/regia.editor.desktop` | The launcher entry (`$XDG_DATA_HOME` replaces `~/.local/share` when set). |
| `~/.local/share/icons/hicolor/<N>x<N>/apps/regia-editor.png` | The icon at 16, 24, 32, 48, 64, 128, 256 and 512 pixels. |
| `~/.local/share/icons/hicolor/scalable/apps/regia-editor.svg` | The scalable icon. |

Then it runs `update-desktop-database` and `gtk-update-icon-cache` when they are installed. The
entry is named after the editor window's app id, `regia.editor` (the Wayland app_id and the X11
`WM_CLASS`), and says `StartupWMClass=regia.editor`, so GNOME, KDE Plasma and other docks show the
icon on the running editor and group it with the launcher. A window rule that matches
`^(regia\..*)$` keeps matching the editor.

The entry passes opened paths to the editor (`Exec=... %F`): a project folder opens that project;
a file inside a project opens the project that holds it. Check the file with
`desktop-file-validate ~/.local/share/applications/regia.editor.desktop` if you have it. A dock
that was running before the install may need a new login to show the icon.

## Windows

The command writes a Start Menu shortcut, `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Regia
Editor.lnk`, and its icon, `%LOCALAPPDATA%\Regia\Editor\regia-editor.ico`. It makes the shortcut
through PowerShell and the shell's `WScript.Shell` object, which every Windows 10 and 11 has. Type
"Regia" in Start to find it; right-click it to pin it to Start or the taskbar.

`regia-editor.exe` itself carries the icon and its version info (ProductName "Regia Editor"), so
Explorer, the taskbar and Alt-Tab show the mark even without the shortcut. The build embeds them
with the Windows SDK's `rc.exe` (MSVC) or MinGW's `windres` (cross builds); a build without a
resource compiler warns and produces an exe without an icon.

## macOS (testing only)

The command builds `~/Applications/Regia Editor.app`: an `Info.plist` (bundle id
`com.piercingxx.regia.editor`, macOS 13 or later), the icon (`regia-editor.icns`) and a small start
script that runs your editor binary. Launchpad, Spotlight and Finder show it with the icon.

The app is not signed. The first time, open `~/Applications` in Finder, right-click **Regia
Editor** and choose **Open**, then **Open** again in the dialog. After that it starts like any
other app. `--uninstall` removes the bundle, and only a bundle this command made (one whose
`Info.plist` names the bundle id above).

## Changing the icon

The whole icon set comes from one file, `crates/regia_editor/assets/icon/regia-editor.svg`.
Replace it, run `cargo xtask editor-icons` to draw the PNGs, the `.ico` and the `.icns` again,
rebuild, and run `regia editor install-launcher` again. `cargo xtask editor-icons --check` tells you
when the drawn files no longer match the SVG.

See also: [Editor launcher (agents)](../agents/editor-launcher.md).
