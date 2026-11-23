# Window and virtual file system

The primary window and the virtual file system through the agent protocol (spec 11 §§ 1 and 4, ADR-7232). Every
method answers on the frame it is called.

## Window

`window.get` takes no parameters and answers the primary window:

| Field | Meaning |
|---|---|
| `title` | Title bar text. |
| `width`, `height` | Requested size in logical pixels. |
| `mode` | `windowed` or `borderless_fullscreen`. |
| `present_mode` | `vsync`, `vsync_relaxed` or `immediate`. |
| `resizable`, `decorations` | The user can resize; the OS draws a title bar and borders. |
| `cursor_visible`, `cursor_grab` | The cursor shows over the window; `none`, `confined` or `locked`. |
| `ime_enabled` | IME text input. |
| `app_id` | The desktop's application id (`null` = `regia.<executable name>`). Set at window creation only. |
| `physical_width`, `physical_height`, `scale_factor` | The current surface, kept up to date by the platform layer. |

`window.set` takes any of `title` (at most 4096 bytes), `width`, `height` (logical pixels, 1 through 16384), `mode`,
`present_mode`, `resizable`, `decorations`, `cursor_visible`, `cursor_grab` and `ime_enabled`. It checks every given
field before it applies any, so a refused call changes nothing. It answers like `window.get` with the requested
values. The platform layer applies them to the OS window in `PostUpdate`, so the physical size changes a frame later:
wait one frame (`play.step` or `wait_until`) before reading `physical_width`.

```json
{ "method": "window.set", "params": { "width": 1280, "height": 720, "mode": "windowed", "cursor_grab": "none" } }
```

## Virtual file system

Paths are `mount://relative/path` with a mount of `assets`, `config`, `user`, `cache` or `logs`; `..`, backslashes,
`:` inside a segment, control characters and Windows device names are refused (`vfs_path_invalid`). The methods read
through the game's own `Vfs` resource, so they reach exactly what the game can read, and `user://`, `cache://` and
`logs://` refuse symbolic links below their root as the game does.

| Method | Parameters | Answer |
|---|---|---|
| `vfs.mounts` | none | `{ mounts: [{ mount, mounted, backend, writable }], pak_mounts }`: `backend` is `dir <path>`, `pak <path> (<n> files)` or the APK's assets; `pak_mounts` says whether `vfs.mount_pak` can open archives. |
| `vfs.exists` | `path` | `{ path, exists }`. |
| `vfs.list` | `path` | `{ path, entries: [{ name, is_dir }] }`, sorted by name. |
| `vfs.read` | `path`, `max_bytes?` | `{ path, size, truncated, text }` when the first `max_bytes` (default 65536, clamped to 1048576) are UTF-8, else `base64` instead of `text`. `size` is the whole file's. |
| `vfs.mount_pak` | `mount`, `pak` | `{ mount, backend }`. Mounts a `.fpak` archive read-only at the mount point, replacing what was there. |

### Pak mounts

`Vfs::mount_pak` serves each manifest path of the pak as a file: reading `assets://textures/rock.ktx2` returns the
stored bytes of the manifest entry `assets/textures/rock.ktx2` (a cook's project-relative path; the leading `assets/`
segment is dropped, other paths are served as written), decompressed (a cooked file for cooked content, the file
itself for a raw one). Directories are the path prefixes. Writes are refused (`vfs_read_only` from the game's
`write_atomic`). `regia_platform` cannot read paks, so `regia_asset` installs the reader (`AssetPlugin` does, or
`regia_asset::install_vfs_pak_mounts()`); without it the call answers `vfs_unsupported`. The `pak` path must lie
under the allowed roots of [Safety](safety.md) (the project, the temp directory, the working directory or an
`output_roots` entry). Cooked assets are still loaded by the asset server (`assets.*`), not through the VFS.

## Errors

| Code | When |
|---|---|
| `window_none` | There is no primary window (a headless or server run). |
| `window_value_unknown` | `mode`, `present_mode` or `cursor_grab` is not one of its names, or `title` is longer than 4096 bytes. |
| `window_size_invalid` | `width` or `height` is not a finite number in 1 through 16384. |
| `vfs_none` | The app has no `Vfs` resource (no `PlatformPlugin`). |
| `vfs_path_invalid` | `path` is not a valid VFS path, or `mount` is not a mount point. |
| `vfs_not_mounted` | Nothing is mounted at the path's mount point. |
| `vfs_not_found` | The file or directory does not exist. |
| `vfs_read_only` | A write to a read-only mount (not reachable through these methods, which only read). |
| `vfs_io` | The backend failed to read (the message says why). |
| `vfs_pak_open` | `vfs.mount_pak`'s file did not open as a pak (missing, not a pak, corrupt table of contents). The previous mount stays. |
| `vfs_unsupported` | No pak reader is installed. |
| `F0002` | A parameter has the wrong type, a required one is missing, a string is over its byte limit, or `pak` lies outside the allowed roots. |

## Rust

`regia_platform::{Window, PrimaryWindow, Vfs, VfsPath, MountPoint, VfsError, install_pak_mount_opener,
pak_mounts_available}` and `regia_asset::{PakVfsMount, install_vfs_pak_mounts}`.
