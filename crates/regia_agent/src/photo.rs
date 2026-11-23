// SPDX-License-Identifier: MPL-2.0
//! `photo.enter`, `photo.set`, `photo.capture`, `photo.status` (ADR-8415).
//!
//! Photo mode is presentation. It never pauses the sim and never writes sim input.
//! `photo_not_allowed` is a competitive match. A live online match opens on the short leash.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_framework::photo_mode::{
    CaptureError, DenyReason, MatchAccess, PhotoMode, capture_size, write_capture_png,
};
use serde_json::{Value, json};

use crate::params;
use crate::protocol::{self, RpcError};

fn mode_mut(world: &mut World) -> impl std::ops::DerefMut<Target = PhotoMode> + '_ {
    if world.get_resource::<PhotoMode>().is_none() {
        world.insert_resource(PhotoMode::default());
    }
    world.resource_mut::<PhotoMode>()
}

fn not_allowed() -> RpcError {
    RpcError::new(
        protocol::PERMISSION_DENIED,
        "photo_not_allowed",
        "photo mode is not allowed in a competitive match",
        "photo.enter without competitive, or use a dead or spectating seat in a non-competitive match",
    )
}

fn inactive() -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "photo_inactive",
        "photo mode is not open",
        "photo.enter first",
    )
}

fn no_directory() -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "photo_no_directory",
        "photo capture has no pictures directory",
        "pass directory, or add PhotoModePlugin so the platform pictures folder is used",
    )
}

fn capture_io(err: &CaptureError) -> RpcError {
    RpcError::new(
        protocol::INTERNAL_ERROR,
        "photo_capture_io",
        err.to_string(),
        "check the directory is writable",
    )
}

fn finite3(params: &Value, name: &str) -> Result<Option<[f32; 3]>, RpcError> {
    let Some(items) = params::opt_array(params, name)? else {
        return Ok(None);
    };
    if items.len() != 3 {
        return Err(RpcError::invalid_params(format!(
            "`{name}` must be [x, y, z] of finite numbers"
        )));
    }
    let mut out = [0.0f32; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        let Some(number) = item.as_f64() else {
            return Err(RpcError::invalid_params(format!(
                "`{name}` must be [x, y, z] of finite numbers"
            )));
        };
        let value = number as f32;
        if !value.is_finite() {
            return Err(RpcError::invalid_params(format!(
                "`{name}` must be [x, y, z] of finite numbers"
            )));
        }
        *slot = value;
    }
    Ok(Some(out))
}

fn grade_index(params: &Value) -> Result<Option<usize>, RpcError> {
    match params.get("grade") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => regia_framework::photo_mode::GRADES
            .iter()
            .position(|item| item.name.eq_ignore_ascii_case(name))
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params("`grade` must be a grade name or index")),
        Some(Value::Number(n)) => {
            let index = n
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`grade` must be a name or an index"))?;
            let index = usize::try_from(index).unwrap_or(usize::MAX);
            if index >= regia_framework::photo_mode::GRADES.len() {
                return Err(RpcError::invalid_params("`grade` index is past the grade list"));
            }
            Ok(Some(index))
        }
        Some(_) => Err(RpcError::invalid_params("`grade` must be a name or an index")),
    }
}

fn access_from(params: &Value) -> Result<MatchAccess, RpcError> {
    Ok(MatchAccess {
        networked: params::opt_bool(params, "networked")?.unwrap_or(false),
        competitive: params::opt_bool(params, "competitive")?.unwrap_or(false),
        paused: params::opt_bool(params, "paused")?.unwrap_or(false),
        training: params::opt_bool(params, "training")?.unwrap_or(false),
        dead: params::opt_bool(params, "dead")?.unwrap_or(false),
        spectating: params::opt_bool(params, "spectating")?.unwrap_or(false),
    })
}

/// `photo.enter`: open photo mode. Every parameter is optional.
///
/// # Errors
/// `photo_not_allowed` when `competitive` is true. `F0002` for a bad parameter.
pub fn photo_enter(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let access = access_from(params)?;
    let anchor = finite3(params, "anchor")?.unwrap_or([0.0; 3]);
    if !regia_framework::photo_mode::allowed(access) {
        return Err(not_allowed());
    }
    let mut mode = mode_mut(world);
    mode.access = access;
    let saved = mode.entry_saved;
    mode.state
        .try_enter(access, saved, anchor)
        .map_err(|reason| match reason {
            DenyReason::Competitive | DenyReason::NetworkedLive => not_allowed(),
        })?;
    mode.hud.hidden = true;
    mode.mark_opened();
    Ok(mode.status_json())
}

/// `photo.set`: camera, leash, tick, game and map. Applied after enter so a later enter
/// resets the eye. Every parameter is optional.
///
/// # Errors
/// `F0002` for a bad parameter.
pub fn photo_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fov = params::opt_f64(params, "fov_deg")?;
    let roll = params::opt_f64(params, "roll_deg")?;
    let exposure = params::opt_f64(params, "exposure_ev")?;
    let focus = params::opt_f64(params, "focus_m")?;
    let aperture = params::opt_f64(params, "aperture_f")?;
    let yaw = params::opt_f64(params, "yaw")?;
    let pitch = params::opt_f64(params, "pitch")?;
    let tick = params::opt_u64(params, "tick")?;
    let game = params::opt_str(params, "game")?;
    let map = params::opt_str(params, "map")?;
    let hires = params::opt_bool(params, "hires")?;
    let width = params::opt_u64(params, "width")?;
    let height = params::opt_u64(params, "height")?;
    let free_radius = params::opt_f64(params, "free_radius_m")?;
    let online_radius = params::opt_f64(params, "online_radius_m")?;
    let anchor = finite3(params, "anchor")?;
    let grade = grade_index(params)?;
    let mut mode = mode_mut(world);
    let limits = mode.state.limits;
    if let Some(fov) = fov {
        mode.state.camera.fov_deg = (fov as f32).clamp(limits.fov_min_deg, limits.fov_max_deg);
    }
    if let Some(roll) = roll {
        mode.state.camera.roll_deg = (roll as f32).clamp(-limits.roll_max_deg, limits.roll_max_deg);
    }
    if let Some(exposure) = exposure {
        mode.state.camera.exposure_ev =
            (exposure as f32).clamp(limits.exposure_min_ev, limits.exposure_max_ev);
    }
    if let Some(focus) = focus {
        mode.state.camera.focus_m = (focus as f32).clamp(limits.focus_min_m, limits.focus_max_m);
    }
    if let Some(aperture) = aperture {
        mode.state.camera.aperture_f = (aperture as f32).clamp(limits.aperture_min_f, limits.aperture_max_f);
    }
    if let Some(yaw) = yaw {
        mode.state.camera.yaw = yaw as f32;
    }
    if let Some(pitch) = pitch {
        let limit = limits.pitch_limit_rad;
        mode.state.camera.pitch = (pitch as f32).clamp(-limit, limit);
    }
    if let Some(tick) = tick {
        mode.tick = tick;
    }
    if let Some(game) = game {
        mode.game = game.to_string();
    }
    if let Some(map) = map {
        mode.map = map.to_string();
    }
    if let Some(hires) = hires {
        mode.hires = hires;
    }
    if let Some(width) = width {
        mode.frame_w = u32::try_from(width).unwrap_or(u32::MAX);
    }
    if let Some(height) = height {
        mode.frame_h = u32::try_from(height).unwrap_or(u32::MAX);
    }
    if let Some(free_radius) = free_radius {
        mode.state.limits.free_radius_m = free_radius as f32;
        mode.state.limits = mode.state.limits.sanitized();
    }
    if let Some(online_radius) = online_radius {
        mode.state.limits.online_radius_m = online_radius as f32;
        mode.state.limits = mode.state.limits.sanitized();
    }
    if let Some(anchor) = anchor {
        mode.state.anchor = anchor;
    }
    if let Some(grade) = grade {
        mode.state.camera.grade = grade;
    }
    Ok(mode.status_json())
}

/// The folder a request's `directory` names (ADR-8795). It follows the output-root rule of
/// `render.out_start`'s `out` (`crate::req_path::allowed_path`): a relative folder joins the project
/// root, an absolute one must lie under the project, the system temp directory or an
/// `output_roots` entry, and `..` or a symlink leading out is refused. An absolute folder under the
/// session's own pictures folder is allowed too. Before this rule a relative folder joined the
/// process's working directory and `..` climbed out of it.
fn capture_dir(world: &World, dir: &str, pictures: Option<&Path>) -> Result<PathBuf, RpcError> {
    match crate::req_path::allowed_path(world, "directory", dir) {
        Ok(path) => Ok(path),
        Err(refusal) => pictures
            .filter(|_| Path::new(dir).is_absolute())
            .and_then(|root| regia_core::project_path::resolve_in_roots(&[root.to_path_buf()], dir).ok())
            .ok_or(refusal),
    }
}

/// `photo.capture`: write one still. The name carries the sim tick.
///
/// # Errors
/// `photo_not_allowed` in a competitive match, `photo_inactive` when photo mode is closed,
/// `photo_no_directory` when no folder is set, `path_outside_project` when `directory` leaves the
/// allowed roots, `photo_capture_io` when the write fails, `F0002` for a bad parameter.
pub fn photo_capture(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tick = params::opt_u64(params, "tick")?;
    let hires = params::opt_bool(params, "hires")?;
    let directory = params::opt_str(params, "directory")?;
    let width = params::opt_u64(params, "width")?;
    let height = params::opt_u64(params, "height")?;
    let pictures = {
        let mode = mode_mut(world);
        if mode.access.competitive && !mode.state.active {
            return Err(not_allowed());
        }
        if !mode.state.active {
            return Err(inactive());
        }
        mode.pictures.clone()
    };
    // The session's pictures folder comes from the host; a request folder is confined.
    let dir = match directory {
        Some("") => return Err(no_directory()),
        Some(dir) => capture_dir(world, dir, pictures.as_deref())?,
        None => pictures.ok_or_else(no_directory)?,
    };
    if dir.as_os_str().is_empty() {
        return Err(no_directory());
    }
    let mut mode = mode_mut(world);
    if let Some(tick) = tick {
        mode.tick = tick;
    }
    let hires = hires.unwrap_or(mode.hires);
    let width = width.map_or(mode.frame_w, |n| u32::try_from(n).unwrap_or(u32::MAX));
    let height = height.map_or(mode.frame_h, |n| u32::try_from(n).unwrap_or(u32::MAX));
    let (w, h) = capture_size(width, height, hires);
    let tick = mode.tick;
    let game = mode.game.clone();
    let map = mode.map.clone();
    match write_capture_png(&dir, &game, &map, tick, w, h) {
        Ok((name, path)) => Ok(json!({
            "name": name,
            "path": path.display().to_string(),
            "width": w,
            "height": h,
            "tick": tick,
            "hires": hires,
        })),
        Err(CaptureError::NoDirectory) => Err(no_directory()),
        Err(err) => Err(capture_io(&err)),
    }
}

/// `photo.status`: the session, including a default closed session.
///
/// # Errors
/// Never, for an empty parameter object.
pub fn photo_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(mode_mut(world).status_json())
}
