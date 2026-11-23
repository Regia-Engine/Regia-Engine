// SPDX-License-Identifier: MPL-2.0
//! `platform.paths`: where this app keeps its files on the host (ADR-525, book
//! `agents/swapchain.md` § Paths, guide topic `swapchain`).
//!
//! The VFS roots (`user://`, `cache://`, `logs://`), the crash report directory, the feedback
//! folder (crash bundles and minidumps: `%LOCALAPPDATA%` on Windows), the Windows `Saved Games`
//! folder and the persistent pipeline cache with the reason it is or is not in use.

use std::path::PathBuf;

use regia_ecs::prelude::*;
use regia_platform::{MountPoint, Vfs, VfsPath};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Why the persistent pipeline cache is or is not in use. `supported` is whether the device's
/// backend has one (wgpu 30: Vulkan only; DX12 and Metal drivers keep their own shader caches);
/// `None` before a device exists. `driver_skipped` is whether a driver rule created the device
/// without it (Intel's Windows Vulkan driver, ADR-3288).
fn cache_state(
    disabled: bool,
    has_directory: bool,
    supported: Option<bool>,
    driver_skipped: bool,
) -> &'static str {
    if disabled {
        "disabled"
    } else if !has_directory {
        "no_directory"
    } else {
        match supported {
            None => "no_device",
            Some(false) => "backend_unsupported",
            Some(true) if driver_skipped => "driver_skipped",
            Some(true) => "active",
        }
    }
}

#[cfg(not(target_os = "android"))]
fn saved_games() -> Option<PathBuf> {
    regia_platform::paths::saved_games_dir()
}

#[cfg(target_os = "android")]
fn saved_games() -> Option<PathBuf> {
    None
}

fn root(world: &World, mount: MountPoint) -> Option<PathBuf> {
    world
        .get_resource::<Vfs>()
        .and_then(|vfs| vfs.host_path(&VfsPath::root(mount)))
}

/// A host path as a JSON string (lossy for a non-UTF-8 path), `null` when unknown.
fn path_json(path: Option<PathBuf>) -> Value {
    path.map_or(Value::Null, |p| Value::String(p.to_string_lossy().into_owned()))
}

/// `platform.paths`: the host directories of this app and the pipeline cache state.
pub(crate) fn paths(world: &mut World) -> Result<Value, RpcError> {
    let device = world.get_resource::<regia_render::RenderDevice>();
    let backend = device.map(|d| d.capabilities.backend);
    let supported = device.map(|d| {
        regia_render::pipeline_cache::PipelineCacheKey::for_adapter(&d.adapter.get_info()).is_some()
    });
    let directory = regia_render::pipeline_cache::directory();
    let has_directory = directory.is_some();
    let disabled =
        regia_core::brand::env_var(regia_render::pipeline_cache::PIPELINE_CACHE_ENV).is_ok_and(|v| v == "0");
    let driver_skipped = device.is_some() && regia_render::pipeline_cache::driver_skipped();
    Ok(json!({
        "os": std::env::consts::OS,
        "user": path_json(root(world, MountPoint::User)),
        "cache": path_json(root(world, MountPoint::Cache)),
        "logs": path_json(root(world, MountPoint::Logs)),
        "crash_reports": path_json(regia_platform::crash::crash_dir()),
        "feedback": path_json(regia_framework::feedback::feedback_folder(world)),
        "saved_games": path_json(saved_games()),
        "pipeline_cache": {
            "dir": path_json(directory),
            "backend": backend.map(|b| b.to_str()),
            "state": cache_state(disabled, has_directory, supported, driver_skipped),
            "skipped": driver_skipped.then_some(regia_render::pipeline_cache::DRIVER_SKIP_REASON),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_state_names_the_first_reason_it_is_off() {
        assert_eq!(cache_state(true, true, Some(true), false), "disabled");
        assert_eq!(cache_state(false, false, Some(true), false), "no_directory");
        assert_eq!(cache_state(false, true, None, false), "no_device");
        assert_eq!(
            cache_state(false, true, Some(false), false),
            "backend_unsupported"
        );
        assert_eq!(cache_state(false, true, Some(true), false), "active");
        // Intel's Windows Vulkan driver (ADR-3288): the backend has a cache, a driver rule left it off.
        assert_eq!(cache_state(false, true, Some(true), true), "driver_skipped");
        // An earlier reason still wins.
        assert_eq!(cache_state(true, true, Some(true), true), "disabled");
        assert_eq!(cache_state(false, true, Some(false), true), "backend_unsupported");
    }

    #[test]
    fn a_bare_world_reports_nulls_not_errors() {
        let mut world = World::new();
        let v = paths(&mut world).expect("never fails"); // invariant: the handler has no error path
        assert_eq!(v["os"], std::env::consts::OS);
        assert_eq!(v["user"], Value::Null);
        assert_eq!(v["cache"], Value::Null);
        assert_eq!(v["pipeline_cache"]["backend"], Value::Null);
        assert_eq!(
            v["pipeline_cache"]["skipped"],
            Value::Null,
            "no device, no driver rule"
        );
        let state = v["pipeline_cache"]["state"].as_str().unwrap_or_default();
        assert!(
            ["disabled", "no_directory", "no_device"].contains(&state),
            "no device: {state}"
        );
    }

    #[test]
    fn mounted_roots_are_reported_as_host_paths() {
        // Nothing is written: the mounts only name directories.
        let dir = std::env::temp_dir().join(format!("regia-agent-paths-{}", std::process::id()));
        let mut vfs = Vfs::default();
        vfs.mount(
            MountPoint::User,
            regia_platform::DirMount::writable(dir.join("user")),
        );
        vfs.mount(
            MountPoint::Cache,
            regia_platform::DirMount::writable(dir.join("cache")),
        );
        let mut world = World::new();
        world.insert_resource(vfs);
        let v = paths(&mut world).expect("never fails"); // invariant: the handler has no error path
        let as_path = |key: &str| v[key].as_str().map(PathBuf::from);
        assert_eq!(as_path("user"), Some(dir.join("user")));
        assert_eq!(as_path("cache"), Some(dir.join("cache")));
        // A mounted user:// keeps its feedback folder there, on every OS (ADR-525).
        assert_eq!(as_path("feedback"), Some(dir.join("user").join("feedback")));
    }
}
