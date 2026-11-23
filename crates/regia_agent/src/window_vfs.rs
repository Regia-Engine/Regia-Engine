// SPDX-License-Identifier: MPL-2.0
//! `window.get`, `window.set`, `vfs.mounts`, `vfs.exists`, `vfs.list`, `vfs.read` and
//! `vfs.mount_pak`: the primary window and the virtual file system (spec 11 §§ 1 and 4, ADR-7232,
//! book/agents/window-and-vfs.md).
//!
//! `window.set` changes the primary window's `Window` component; the platform layer applies it to
//! the OS window in `PostUpdate`, so `window.get` right after the call returns the requested values
//! and the physical size follows a frame later. The `vfs.*` methods read through the `Vfs` resource
//! with validated `mount://` paths, so they reach exactly what the game itself can read.

use regia_app::prelude::*;
use regia_platform::{
    CursorGrab, MountPoint, PresentMode, PrimaryWindow, Vfs, VfsError, VfsPath, Window, WindowMode,
};
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

/// Largest `vfs.read` answer (`max_bytes` is clamped to it).
pub const VFS_READ_MAX_BYTES: u64 = 1 << 20;

/// `vfs.read`'s `max_bytes` when omitted.
pub const VFS_READ_DEFAULT_BYTES: u64 = 64 * 1024;

/// Largest logical window edge `window.set` accepts.
const MAX_WINDOW_EDGE: f64 = 16_384.0;

fn mode_name(mode: WindowMode) -> &'static str {
    match mode {
        WindowMode::Windowed => "windowed",
        WindowMode::BorderlessFullscreen => "borderless_fullscreen",
    }
}

fn present_name(mode: PresentMode) -> &'static str {
    match mode {
        PresentMode::Vsync => "vsync",
        PresentMode::VsyncRelaxed => "vsync_relaxed",
        PresentMode::Immediate => "immediate",
    }
}

fn grab_name(grab: CursorGrab) -> &'static str {
    match grab {
        CursorGrab::None => "none",
        CursorGrab::Confined => "confined",
        CursorGrab::Locked => "locked",
    }
}

fn unknown_value(param: &str, value: &str, allowed: &str) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "window_value_unknown",
        format!("`{param}` {value:?} is not one of {allowed}"),
        format!("pass one of {allowed}"),
    )
}

fn primary_window(world: &mut World) -> Result<Entity, RpcError> {
    let mut q = world.query_filtered::<Entity, (With<PrimaryWindow>, With<Window>)>();
    q.iter(world).next().ok_or_else(|| {
        RpcError::new(
            protocol::UNSUPPORTED,
            "window_none",
            "there is no primary window (a headless or server run)",
            "window.* needs a windowed run; vfs.* works headless",
        )
    })
}

fn window_json(w: &Window) -> Value {
    json!({
        "title": w.title,
        "width": w.logical_size.0,
        "height": w.logical_size.1,
        "mode": mode_name(w.mode),
        "present_mode": present_name(w.present_mode),
        "resizable": w.resizable,
        "decorations": w.decorations,
        "cursor_visible": w.cursor_visible,
        "cursor_grab": grab_name(w.cursor_grab),
        "ime_enabled": w.ime_enabled,
        "app_id": w.app_id,
        "physical_width": w.physical_size.0,
        "physical_height": w.physical_size.1,
        "scale_factor": w.scale_factor,
    })
}

/// `window.get`: the primary window's settings and current surface size.
pub(crate) fn window_get(world: &mut World) -> Result<Value, RpcError> {
    let entity = primary_window(world)?;
    let window = world
        .get::<Window>(entity)
        .ok_or_else(|| RpcError::internal("the primary window lost its Window component"))?;
    Ok(window_json(window))
}

fn opt_bool(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RpcError::wrong_type(key, "a boolean")),
    }
}

fn opt_str<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(RpcError::wrong_type(key, "a string")),
    }
}

fn opt_edge(params: &Value, key: &str) -> Result<Option<f64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_f64().ok_or_else(|| RpcError::wrong_type(key, "a number"))?;
            if !n.is_finite() || n < 1.0 || n > MAX_WINDOW_EDGE {
                return Err(RpcError::new(
                    protocol::INVALID_PARAMS,
                    "window_size_invalid",
                    format!("`{key}` {n} is outside 1..={MAX_WINDOW_EDGE} logical pixels"),
                    "pass a logical size between 1 and 16384",
                ));
            }
            Ok(Some(n))
        }
    }
}

/// `window.set { title?, width?, height?, mode?, present_mode?, resizable?, decorations?,
/// cursor_visible?, cursor_grab?, ime_enabled? }`: every given field is checked before any is
/// applied; returns the window as `window.get` does.
pub(crate) fn window_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = primary_window(world)?;
    let title = opt_str(params, "title")?;
    if title.is_some_and(|t| t.len() > 4096) {
        return Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "window_value_unknown",
            "`title` is longer than 4096 bytes",
            "pass a shorter title",
        ));
    }
    let width = opt_edge(params, "width")?;
    let height = opt_edge(params, "height")?;
    let mode = match opt_str(params, "mode")? {
        None => None,
        Some("windowed") => Some(WindowMode::Windowed),
        Some("borderless_fullscreen") => Some(WindowMode::BorderlessFullscreen),
        Some(other) => return Err(unknown_value("mode", other, "windowed, borderless_fullscreen")),
    };
    let present_mode = match opt_str(params, "present_mode")? {
        None => None,
        Some("vsync") => Some(PresentMode::Vsync),
        Some("vsync_relaxed") => Some(PresentMode::VsyncRelaxed),
        Some("immediate") => Some(PresentMode::Immediate),
        Some(other) => {
            return Err(unknown_value(
                "present_mode",
                other,
                "vsync, vsync_relaxed, immediate",
            ));
        }
    };
    let cursor_grab = match opt_str(params, "cursor_grab")? {
        None => None,
        Some("none") => Some(CursorGrab::None),
        Some("confined") => Some(CursorGrab::Confined),
        Some("locked") => Some(CursorGrab::Locked),
        Some(other) => return Err(unknown_value("cursor_grab", other, "none, confined, locked")),
    };
    let resizable = opt_bool(params, "resizable")?;
    let decorations = opt_bool(params, "decorations")?;
    let cursor_visible = opt_bool(params, "cursor_visible")?;
    let ime_enabled = opt_bool(params, "ime_enabled")?;
    let mut window = world
        .get_mut::<Window>(entity)
        .ok_or_else(|| RpcError::internal("the primary window lost its Window component"))?;
    if let Some(t) = title {
        window.title = t.to_owned();
    }
    if let Some(w) = width {
        window.logical_size.0 = w;
    }
    if let Some(h) = height {
        window.logical_size.1 = h;
    }
    if let Some(m) = mode {
        window.mode = m;
    }
    if let Some(p) = present_mode {
        window.present_mode = p;
    }
    if let Some(g) = cursor_grab {
        window.cursor_grab = g;
    }
    if let Some(b) = resizable {
        window.resizable = b;
    }
    if let Some(b) = decorations {
        window.decorations = b;
    }
    if let Some(b) = cursor_visible {
        window.cursor_visible = b;
    }
    if let Some(b) = ime_enabled {
        window.ime_enabled = b;
    }
    Ok(window_json(&window))
}

fn vfs(world: &World) -> Result<&Vfs, RpcError> {
    world.get_resource::<Vfs>().ok_or_else(|| {
        RpcError::new(
            protocol::UNSUPPORTED,
            "vfs_none",
            "this app has no Vfs resource",
            "add regia_platform's PlatformPlugin (DefaultPlugins has it)",
        )
    })
}

fn vfs_path(params: &Value) -> Result<VfsPath, RpcError> {
    let text = opt_str(params, "path")?.ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    VfsPath::parse(text).map_err(|e| {
        RpcError::new(
            protocol::INVALID_PARAMS,
            "vfs_path_invalid",
            format!("{text:?}: {e}"),
            "use mount://relative/path with a mount of assets, config, user, cache or logs and no `..`",
        )
    })
}

fn vfs_error(err: VfsError) -> RpcError {
    let message = err.to_string();
    match err {
        VfsError::InvalidPath { .. } => RpcError::new(
            protocol::INVALID_PARAMS,
            "vfs_path_invalid",
            message,
            "use mount://relative/path",
        ),
        VfsError::NotMounted { .. } => RpcError::new(
            protocol::NOT_FOUND,
            "vfs_not_mounted",
            message,
            "vfs.mounts lists what is mounted",
        ),
        VfsError::NotFound { .. } => RpcError::new(
            protocol::NOT_FOUND,
            "vfs_not_found",
            message,
            "vfs.list shows a directory's entries",
        ),
        VfsError::ReadOnly { .. } => RpcError::new(
            protocol::PERMISSION_DENIED,
            "vfs_read_only",
            message,
            "only user:// and cache:// are writable",
        ),
        VfsError::Unsupported(_) => RpcError::new(
            protocol::UNSUPPORTED,
            "vfs_unsupported",
            message,
            "the asset plugin installs the pak reader",
        ),
        VfsError::PakOpen { .. } => RpcError::new(
            protocol::INVALID_PARAMS,
            "vfs_pak_open",
            message,
            "pass the path of a .fpak archive",
        ),
        VfsError::Io { .. } => RpcError::new(protocol::INTERNAL_ERROR, "vfs_io", message, "see logs.read"),
    }
}

/// `vfs.mounts`: every mount point, whether something is mounted there and what backs it.
pub(crate) fn mounts(world: &World) -> Result<Value, RpcError> {
    let vfs = vfs(world)?;
    let rows: Vec<Value> = MountPoint::ALL
        .iter()
        .map(|point| {
            let backend = vfs.describe_mount(*point);
            json!({
                "mount": point.as_str(),
                "mounted": backend.is_some(),
                "backend": backend,
                "writable": point.is_writable(),
            })
        })
        .collect();
    Ok(json!({ "mounts": rows, "pak_mounts": regia_platform::pak_mounts_available() }))
}

/// `vfs.exists { path }`.
pub(crate) fn exists(world: &World, params: &Value) -> Result<Value, RpcError> {
    let path = vfs_path(params)?;
    let vfs = vfs(world)?;
    Ok(json!({ "path": path.to_string(), "exists": vfs.exists(&path) }))
}

/// `vfs.list { path }`: a directory's entries, sorted by name.
pub(crate) fn list(world: &World, params: &Value) -> Result<Value, RpcError> {
    let path = vfs_path(params)?;
    let entries = vfs(world)?.list_dir(&path).map_err(vfs_error)?;
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|e| json!({ "name": e.name, "is_dir": e.is_dir }))
        .collect();
    Ok(json!({ "path": path.to_string(), "entries": entries }))
}

/// `vfs.read { path, max_bytes? }`: the first `max_bytes` (default 65536, at most 1 MiB) of a
/// file, as `text` when that prefix is UTF-8, else as `base64`.
pub(crate) fn read(world: &World, params: &Value) -> Result<Value, RpcError> {
    let path = vfs_path(params)?;
    let max = match params.get("max_bytes") {
        None | Some(Value::Null) => VFS_READ_DEFAULT_BYTES,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| RpcError::wrong_type("max_bytes", "a non-negative integer"))?
            .min(VFS_READ_MAX_BYTES),
    };
    let bytes = vfs(world)?.read_sync(&path).map_err(vfs_error)?;
    let size = bytes.len() as u64;
    let shown = &bytes[..usize::try_from(size.min(max)).unwrap_or(bytes.len())];
    let mut out = Map::new();
    out.insert("path".into(), Value::String(path.to_string()));
    out.insert("size".into(), json!(size));
    out.insert("truncated".into(), Value::Bool(size > max));
    match std::str::from_utf8(shown) {
        Ok(text) => out.insert("text".into(), Value::String(text.to_owned())),
        Err(_) => out.insert("base64".into(), Value::String(crate::image::base64(shown))),
    };
    Ok(Value::Object(out))
}

/// `vfs.mount_pak { mount, pak }`: mounts a `.fpak` archive read-only at `mount`. `pak` must lie
/// under the allowed roots (the project, the temp directory, the working directory or an
/// `output_roots` entry).
pub(crate) fn mount_pak(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mount = opt_str(params, "mount")?.ok_or_else(|| RpcError::invalid_params("`mount` is required"))?;
    let point = MountPoint::ALL
        .iter()
        .copied()
        .find(|p| p.as_str() == mount.trim_end_matches("://"))
        .ok_or_else(|| {
            RpcError::new(
                protocol::INVALID_PARAMS,
                "vfs_path_invalid",
                format!("`mount` {mount:?} is not a mount point"),
                "pass assets, config, user, cache or logs",
            )
        })?;
    let pak = opt_str(params, "pak")?.ok_or_else(|| RpcError::invalid_params("`pak` is required"))?;
    let pak = crate::req_path::allowed_path(world, "pak", pak)?;
    let mut vfs = world.get_resource_mut::<Vfs>().ok_or_else(|| {
        RpcError::new(
            protocol::UNSUPPORTED,
            "vfs_none",
            "this app has no Vfs resource",
            "add regia_platform's PlatformPlugin (DefaultPlugins has it)",
        )
    })?;
    vfs.mount_pak(point, &pak).map_err(vfs_error)?;
    Ok(json!({ "mount": point.as_str(), "backend": vfs.describe_mount(point) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        crate::dispatch::call_in_process(world, method, params)
            .unwrap_or_else(|| panic!("{method} must answer synchronously"))
    }

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn window_get_and_set_round_trip_through_the_dispatcher() {
        let mut world = World::new();
        assert_eq!(
            code(&call(&mut world, "window.get", json!({})).expect_err("no window")),
            "window_none"
        );
        world.spawn((Window::default(), PrimaryWindow));
        let got = call(&mut world, "window.get", json!({})).unwrap_or_else(|e| panic!("get: {e:?}"));
        assert_eq!(got["mode"], "windowed");
        assert_eq!(got["width"], 1280.0);
        let set = call(
            &mut world,
            "window.set",
            json!({ "title": "Arena", "width": 800, "height": 600, "mode": "borderless_fullscreen",
                    "present_mode": "immediate", "cursor_grab": "locked", "cursor_visible": false }),
        )
        .unwrap_or_else(|e| panic!("set: {e:?}"));
        assert_eq!(set["title"], "Arena");
        assert_eq!(
            (set["width"].as_f64(), set["height"].as_f64()),
            (Some(800.0), Some(600.0))
        );
        assert_eq!(set["mode"], "borderless_fullscreen");
        assert_eq!(set["present_mode"], "immediate");
        assert_eq!(set["cursor_grab"], "locked");
        assert_eq!(set["cursor_visible"], false);
        assert_eq!(call(&mut world, "window.get", json!({})).ok(), Some(set.clone()));
        // A bad field changes nothing.
        for (bad, want) in [
            (
                json!({ "title": "x", "mode": "fullscreen" }),
                "window_value_unknown",
            ),
            (json!({ "title": "x", "width": 0 }), "window_size_invalid"),
            (json!({ "title": "x", "height": 1e9 }), "window_size_invalid"),
        ] {
            let err = call(&mut world, "window.set", bad).expect_err("refused");
            assert_eq!(code(&err), want);
        }
        assert_eq!(call(&mut world, "window.get", json!({})).ok(), Some(set));
    }

    #[test]
    fn vfs_methods_read_through_the_mounts() {
        let dir = regia_test_utils::unique_temp_dir("agent-vfs");
        std::fs::create_dir_all(dir.join("cfg")).unwrap_or_else(|e| panic!("{e}"));
        regia_test_utils::write_atomic(&dir.join("cfg/game.ron"), b"(lives: 3)")
            .unwrap_or_else(|e| panic!("{e}"));
        regia_test_utils::write_atomic(&dir.join("blob.bin"), &[0xff, 0xfe, 0x00, 0x01])
            .unwrap_or_else(|e| panic!("{e}"));
        let mut world = World::new();
        assert_eq!(
            code(&call(&mut world, "vfs.mounts", json!({})).expect_err("no vfs")),
            "vfs_none"
        );
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Assets, regia_platform::DirMount::read_only(&dir));
        world.insert_resource(vfs);

        let mounts = call(&mut world, "vfs.mounts", json!({})).unwrap_or_else(|e| panic!("{e:?}"));
        let assets = &mounts["mounts"][0];
        assert_eq!(
            (assets["mount"].as_str(), assets["mounted"].as_bool()),
            (Some("assets"), Some(true))
        );
        let listed =
            call(&mut world, "vfs.list", json!({ "path": "assets://" })).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            listed["entries"],
            json!([{ "name": "blob.bin", "is_dir": false }, { "name": "cfg", "is_dir": true }])
        );
        let text = call(&mut world, "vfs.read", json!({ "path": "assets://cfg/game.ron" }))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            (text["text"].as_str(), text["size"].as_u64()),
            (Some("(lives: 3)"), Some(10))
        );
        let cut = call(
            &mut world,
            "vfs.read",
            json!({ "path": "assets://cfg/game.ron", "max_bytes": 4 }),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            (cut["text"].as_str(), cut["truncated"].as_bool()),
            (Some("(liv"), Some(true))
        );
        let blob = call(&mut world, "vfs.read", json!({ "path": "assets://blob.bin" }))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(blob["base64"], crate::image::base64(&[0xff, 0xfe, 0x00, 0x01]));
        let yes = call(&mut world, "vfs.exists", json!({ "path": "assets://cfg" }))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(yes["exists"], true);
        for (params, want) in [
            (json!({ "path": "assets://../etc/passwd" }), "vfs_path_invalid"),
            (json!({ "path": "assets://missing.txt" }), "vfs_not_found"),
            (json!({ "path": "user://saves/a.sav" }), "vfs_not_mounted"),
        ] {
            let err = call(&mut world, "vfs.read", params).expect_err("refused");
            assert_eq!(code(&err), want);
        }
        let err = call(
            &mut world,
            "vfs.mount_pak",
            json!({ "mount": "assets", "pak": dir.join("blob.bin").to_string_lossy() }),
        )
        .expect_err("not a pak");
        assert!(
            matches!(code(&err).as_str(), "vfs_pak_open" | "vfs_unsupported"),
            "{err:?}"
        );
    }
}
