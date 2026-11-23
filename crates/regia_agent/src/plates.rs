// SPDX-License-Identifier: MPL-2.0
//! `seeing.capture_plate` (spec 27 § 5, ADR-2463, book `agents/plates.md`): renders one shot of a
//! look review to PNG or EXR files plus a JSON sidecar.
//!
//! A plate is a one-frame render-out job (`render.out_start`) taken after the shot is applied: the
//! camera pose or named camera, the level's time of day and the render profile preset are set,
//! `warmup_frames` frames settle the scene, and the frame (and any AOV files) is written
//! atomically (`<name>.part`, then rename) by the render-out writer. The same shot on the same
//! build gives the same file hashes (lavapipe: byte for byte); nothing reads a wall clock.
//!
//! The call returns at once with a `plate` id; polling it with `seeing.capture_plate {plate}`
//! reports the render-out status and, once the job is done, writes the sidecar
//! `<name>.plate.json` (tmp file + rename) with the engine version, profile, the camera as
//! rendered, and the BLAKE3 hash and size of every file. A shot comes inline (`shot`) or from a
//! `.shot.ron` list (`shots_file` + `name`, `regia_agent::shot_list`).
//!
//! Every error carries a lowercase `data.reason`: `bad_shot`, `shot_list_invalid`,
//! `shot_not_found`, `path_outside_project`, `camera_not_found`, `unknown_profile`,
//! `no_renderer`, `plate_not_found`, `plate_failed`, plus the render-out `F0018` busy answer.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_render::{Camera, RenderProfilePreset, RenderSettings};
use regia_scene::level::{LevelSettings, LevelTimeOfDay};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};
use crate::reference_overlay::reasoned;
use crate::shot_list::{Shot, ShotCamera, ShotList, ShotTime};

/// Sidecar schema id.
pub const PLATE_SIDECAR_SCHEMA: &str = "regia.plate/1";

/// Plate jobs kept for polling.
const KEEP_PLATES: usize = 32;

/// Warm-up frames when a shot names none.
const DEFAULT_WARMUP: u32 = 8;

/// Size when a shot names none.
const DEFAULT_SIZE: (u32, u32) = (1280, 720);

/// One started plate.
struct PlateJob {
    id: u64,
    render_job: u64,
    shot: Shot,
    stem: String,
    tag: Option<String>,
    out_dir: PathBuf,
    /// The camera entity the shot drives (named, or the posed primary camera); the sidecar
    /// records this one.
    camera: Option<Entity>,
    /// The camera (`camera_json`) and the profile preset as the frame was delivered
    /// (`frame_delivered`): a later plate or pose must not show up in this plate's sidecar.
    rendered: Option<(Value, Option<Value>)>,
    /// The sidecar once written.
    sidecar: Option<Value>,
}

/// The plates started in this run.
#[derive(Resource, Default)]
pub(crate) struct PlateJobs {
    next: u64,
    jobs: Vec<PlateJob>,
}

fn bad_shot(message: impl Into<String>) -> RpcError {
    reasoned(
        protocol::INVALID_PARAMS,
        "F0002",
        "bad_shot",
        message,
        "see book/agents/plates.md for the shot fields",
    )
}

/// `seeing.capture_plate`: starts a plate (`shot` or `shots_file` + `name`), or polls one
/// (`plate`).
pub(crate) fn capture_plate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(plate) = params.get("plate") {
        let id = plate
            .as_u64()
            .ok_or_else(|| bad_shot("`plate` must be the integer a start call returned"))?;
        return poll(world, id);
    }
    start(world, params)
}

fn json_f32(v: &Value) -> Option<f32> {
    v.as_f64().filter(|f| f.is_finite()).map(|f| f as f32)
}

fn vec3(v: Option<&Value>, key: &str) -> Result<(f32, f32, f32), RpcError> {
    let list = v
        .and_then(Value::as_array)
        .filter(|a| a.len() == 3)
        .ok_or_else(|| bad_shot(format!("`camera.{key}` must be an array of three numbers")))?;
    let n =
        |i: usize| json_f32(&list[i]).ok_or_else(|| bad_shot(format!("`camera.{key}` holds a non-number")));
    Ok((n(0)?, n(1)?, n(2)?))
}

/// Builds a [`Shot`] from the `shot` object of the request.
fn shot_from_json(value: &Value) -> Result<Shot, RpcError> {
    let object = value
        .as_object()
        .ok_or_else(|| bad_shot("`shot` must be an object"))?;
    const KEYS: [&str; 9] = [
        "name",
        "level",
        "camera",
        "time_of_day",
        "profile",
        "width",
        "height",
        "warmup_frames",
        "aovs",
    ];
    if let Some(unknown) = object
        .keys()
        .find(|k| !KEYS.contains(&k.as_str()) && k.as_str() != "format")
    {
        return Err(bad_shot(format!("unknown shot field `{unknown}`")));
    }
    let text = |key: &str| -> Result<Option<String>, RpcError> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_str()
                .map(|s| Some(s.to_string()))
                .ok_or_else(|| bad_shot(format!("`{key}` must be a string"))),
        }
    };
    let number = |key: &str| -> Result<Option<u32>, RpcError> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| bad_shot(format!("`{key}` must be a non-negative integer"))),
        }
    };
    // A key given with a wrong value is refused (ADR-5597): it used to read as absent, so the
    // plate silently rendered the scene's own value instead of the one asked for.
    let opt_f32 = |v: &Value, key: &str, what: &str| -> Result<Option<f32>, RpcError> {
        match v.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(x) => json_f32(x)
                .map(Some)
                .ok_or_else(|| bad_shot(format!("`{what}` must be a finite number"))),
        }
    };
    let camera = match object.get("camera") {
        None | Some(Value::Null) => None,
        Some(c) => {
            if let Some(name) = c.get("name").and_then(Value::as_str) {
                Some(ShotCamera::Named(name.to_string()))
            } else {
                Some(ShotCamera::Pose {
                    eye: vec3(c.get("eye"), "eye")?,
                    target: vec3(c.get("target"), "target")?,
                    fov_y_deg: opt_f32(c, "fov_y_deg", "camera.fov_y_deg")?,
                })
            }
        }
    };
    let time_of_day = match object.get("time_of_day") {
        None | Some(Value::Null) => None,
        Some(t) => Some(ShotTime {
            hour: t
                .get("hour")
                .and_then(json_f32)
                .ok_or_else(|| bad_shot("`time_of_day.hour` (number) is required"))?,
            day_of_year: match t.get("day_of_year") {
                None | Some(Value::Null) => None,
                Some(d) => Some(
                    d.as_u64()
                        .and_then(|d| u16::try_from(d).ok())
                        .ok_or_else(|| bad_shot("`time_of_day.day_of_year` must be in 1..=365"))?,
                ),
            },
            latitude_deg: opt_f32(t, "latitude_deg", "time_of_day.latitude_deg")?,
        }),
    };
    let aovs = match object.get("aovs") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) => list
            .iter()
            .map(|a| {
                a.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| bad_shot("`aovs` entries must be strings"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(bad_shot("`aovs` must be an array of strings")),
    };
    let shot = Shot {
        name: text("name")?.ok_or_else(|| bad_shot("`shot.name` (string) is required"))?,
        level: text("level")?,
        camera,
        time_of_day,
        profile: text("profile")?,
        width: number("width")?,
        height: number("height")?,
        warmup_frames: number("warmup_frames")?,
        aovs,
        format: text("format")?,
    };
    shot.validate().map_err(|e| bad_shot(e.to_string()))?;
    Ok(shot)
}

/// The shot a request names: inline, or `name` of a `.shot.ron` list.
fn resolve_shot(world: &World, params: &Value) -> Result<Shot, RpcError> {
    if let Some(value) = params.get("shot") {
        return shot_from_json(value);
    }
    let Some(file) = params.get("shots_file").and_then(Value::as_str) else {
        return Err(bad_shot(
            "give `shot` (an object) or `shots_file` (a .shot.ron path) with `name`",
        ));
    };
    let path = crate::req_path::project_path(world, "shots_file", file)?;
    // Capped and regular-file only (ADR-5597): a FIFO planted at the path blocked the world
    // thread, and a huge file was read whole.
    let text = crate::req_path::read_file_capped(&path, crate::req_path::MAX_READ_BYTES)
        .and_then(|bytes| {
            String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })
        .map_err(|e| {
            reasoned(
                protocol::NOT_FOUND,
                "F0003",
                "shot_list_invalid",
                format!("cannot read `shots_file`: {e}"),
                "give a project-relative .shot.ron path",
            )
        })?;
    let list = ShotList::from_ron(&text).map_err(|e| {
        reasoned(
            protocol::INVALID_PARAMS,
            "F0002",
            "shot_list_invalid",
            e.to_string(),
            "see docs/formats/shot-ron.md",
        )
    })?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_shot("`name` (a shot of the list) is required with `shots_file`"))?;
    list.shot(name).cloned().ok_or_else(|| {
        reasoned(
            protocol::NOT_FOUND,
            "F0003",
            "shot_not_found",
            format!("no shot named {name:?} in the list"),
            format!(
                "the list holds: {}",
                list.shots
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    })
}

/// The camera entity a shot drives: the one named, else the lowest `order` (then entity).
fn pick_camera(world: &mut World, named: Option<&str>) -> Option<Entity> {
    let mut query = world.query::<(Entity, &Camera, Option<&regia_scene::components::Name>)>();
    let mut cameras: Vec<(i32, Entity)> = query
        .iter(world)
        .filter(|(_, _, name)| named.is_none_or(|n| name.is_some_and(|name| name.as_str() == n)))
        .map(|(entity, camera, _)| (camera.order, entity))
        .collect();
    cameras.sort();
    cameras.first().map(|(_, e)| *e)
}

fn start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let shot = resolve_shot(world, params)?;
    let tag = match params.get("tag") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let tag = v.as_str().filter(|t| crate::shot_list::valid_shot_name(t));
            Some(
                tag.ok_or_else(|| bad_shot("`tag` must be 1..=64 of a-z, 0-9, `_`, `-`"))?
                    .to_string(),
            )
        }
    };
    let stem = match &tag {
        Some(tag) => format!("{}-{tag}", shot.name),
        None => shot.name.clone(),
    };
    // A wrong-typed `out` is refused, not read as the default folder (ADR-2622).
    let out_text = crate::params::opt_str(params, "out")?.unwrap_or("plates");
    let out_dir = crate::render_out::confine_path(world, "out", Path::new(out_text))?;

    if world.get_resource::<RenderSettings>().is_none() {
        return Err(reasoned(
            protocol::UNSUPPORTED,
            "F0011",
            "no_renderer",
            "no renderer in this app",
            "add regia_render::RenderPlugin",
        ));
    }
    let preset = match shot.profile.as_deref() {
        None => None,
        Some(name) => Some(RenderProfilePreset::from_name(name).ok_or_else(|| {
            reasoned(
                protocol::INVALID_PARAMS,
                "F0002",
                "unknown_profile",
                format!("unknown profile {name:?}"),
                "use a name render.get_profile lists",
            )
        })?),
    };
    let camera_entity = match &shot.camera {
        None => None,
        Some(ShotCamera::Named(name)) => Some(pick_camera(world, Some(name)).ok_or_else(|| {
            reasoned(
                protocol::NOT_FOUND,
                "F0003",
                "camera_not_found",
                format!("no camera entity named {name:?}"),
                "scene.query_entities lists entities with a Camera",
            )
        })?),
        Some(ShotCamera::Pose { .. }) => Some(pick_camera(world, None).ok_or_else(|| {
            reasoned(
                protocol::NOT_FOUND,
                "F0003",
                "camera_not_found",
                "the scene has no camera to move",
                "spawn a Camera first",
            )
        })?),
    };

    let (width, height) = (
        shot.width.unwrap_or(DEFAULT_SIZE.0),
        shot.height.unwrap_or(DEFAULT_SIZE.1),
    );
    let warmup = shot.warmup_frames.unwrap_or(DEFAULT_WARMUP);
    let mut start = json!({
        "out": out_dir.display().to_string(),
        "name": stem,
        "frames": 1,
        "width": width,
        "height": height,
        "fps": 30,
        "hud": false,
        "format": shot.format.as_deref().unwrap_or("png"),
        "warmup_frames": warmup,
        "settle_frames": warmup.min(4),
    });
    if !shot.aovs.is_empty() {
        start["aovs"] = json!(shot.aovs);
    }
    // The job takes the clock first: a refusal (busy, bad size) leaves the scene untouched.
    let started = crate::render_out::start(world, &start)?;
    let render_job = started["job"].as_u64().unwrap_or(0);

    if let Some(preset) = preset
        && let Some(mut settings) = world.get_resource_mut::<RenderSettings>()
    {
        settings.apply_preset(preset);
    }
    if let Some(time) = &shot.time_of_day {
        let mut settings = world.get_resource_or_insert_with(LevelSettings::default);
        let clock = settings.atmosphere.time_of_day.unwrap_or_default();
        settings.atmosphere.time_of_day = Some(LevelTimeOfDay {
            hour: time.hour,
            day_of_year: time.day_of_year.unwrap_or(clock.day_of_year),
            latitude_deg: time.latitude_deg.unwrap_or(clock.latitude_deg),
        });
    }
    if let (
        Some(ShotCamera::Pose {
            eye,
            target,
            fov_y_deg,
        }),
        Some(entity),
    ) = (&shot.camera, camera_entity)
    {
        apply_pose(world, entity, *eye, *target, *fov_y_deg);
    }
    if let (Some(ShotCamera::Named(_)), Some(entity)) = (&shot.camera, camera_entity) {
        make_frontmost(world, entity);
    }

    let jobs = world.get_resource_or_insert_with(PlateJobs::default).into_inner();
    jobs.next += 1;
    let id = jobs.next;
    jobs.jobs.push(PlateJob {
        id,
        render_job,
        shot: shot.clone(),
        stem: stem.clone(),
        tag,
        out_dir: out_dir.clone(),
        camera: camera_entity,
        rendered: None,
        sidecar: None,
    });
    if jobs.jobs.len() > KEEP_PLATES {
        jobs.jobs.remove(0);
    }
    Ok(json!({
        "plate": id,
        "job": render_job,
        "status": "running",
        "name": stem,
        "width": width,
        "height": height,
        "warmup_frames": warmup,
        "poll": "seeing.capture_plate {plate} until status is done; the sidecar is written then",
    }))
}

/// Makes `entity` the camera a named shot's picture comes from. The renderer draws every window
/// view onto the target in `order` (lowest first), so a full-viewport camera drawn later covers
/// the ones before it: the named camera's `order` rises above every other camera's when another
/// camera has the same or a higher one (equal orders keep extraction order, so a tie is not
/// enough). The change stays after the plate, as a pose does.
fn make_frontmost(world: &mut World, entity: Entity) {
    let mut query = world.query::<(Entity, &Camera)>();
    let highest_other = query
        .iter(world)
        .filter(|(e, _)| *e != entity)
        .map(|(_, camera)| camera.order)
        .max();
    if let Some(highest) = highest_other
        && let Some(mut camera) = world.get_mut::<Camera>(entity)
        && camera.order <= highest
    {
        camera.order = highest.saturating_add(1);
    }
}

fn apply_pose(
    world: &mut World,
    entity: Entity,
    eye: (f32, f32, f32),
    target: (f32, f32, f32),
    fov_deg: Option<f32>,
) {
    use regia_core::math::{Mat3, Quat, Vec3};
    let eye = Vec3::new(eye.0, eye.1, eye.2);
    let target = Vec3::new(target.0, target.1, target.2);
    let forward = (target - eye).normalize_or_zero();
    // Looking along -Z with +Y up; a straight-up or straight-down look uses +Z as up instead.
    let up = if forward.y.abs() > 0.999 { Vec3::Z } else { Vec3::Y };
    // The camera looks along its -Z with +Y up: right = forward x up, true up = right x forward.
    let right = forward.cross(up).normalize_or_zero();
    let true_up = right.cross(forward);
    let rotation = Quat::from_mat3(&Mat3::from_cols(right, true_up, -forward));
    let transform = Transform {
        translation: eye,
        rotation,
        scale: Vec3::ONE,
    };
    let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
        return;
    };
    entity_mut.insert((transform, GlobalTransform(transform.compute_affine())));
    if let Some(fov) = fov_deg
        && let Some(mut camera) = entity_mut.get_mut::<Camera>()
    {
        camera.fov_y = fov.to_radians();
    }
}

fn poll(world: &mut World, id: u64) -> Result<Value, RpcError> {
    let Some(jobs) = world.get_resource::<PlateJobs>() else {
        return Err(plate_not_found(id));
    };
    let Some(job) = jobs.jobs.iter().find(|j| j.id == id) else {
        return Err(plate_not_found(id));
    };
    if let Some(sidecar) = &job.sidecar {
        return Ok(sidecar_answer(job, sidecar, "done"));
    }
    let (render_job, stem, out_dir) = (job.render_job, job.stem.clone(), job.out_dir.clone());
    let progress = crate::render_out::progress(world, &json!({ "job": render_job, "hashes": true }))?;
    let status = progress["status"].as_str().unwrap_or("running").to_string();
    match status.as_str() {
        "done" => {}
        "running" | "cancelling" => {
            return Ok(json!({
                "plate": id,
                "job": render_job,
                "status": "running",
                "phase": progress["phase"],
                "name": stem,
            }));
        }
        other => {
            return Err(reasoned(
                protocol::INTERNAL_ERROR,
                "F0001",
                "plate_failed",
                format!("the render-out job ended {other}: {}", progress["error"]),
                "render.out_progress shows the job",
            ));
        }
    }
    let (camera_entity, rendered) = world
        .resource::<PlateJobs>()
        .jobs
        .iter()
        .find(|j| j.id == id)
        .map(|j| (j.camera, j.rendered.clone()))
        .unwrap_or_default();
    // A job that delivered no frame through `frame_delivered` (none known) falls back to now.
    let (camera, profile) = match rendered {
        Some(rendered) => rendered,
        None => rendered_state(world, camera_entity),
    };
    let hash = progress["hashes"]["hashes"][0].clone();
    let shot = world
        .resource::<PlateJobs>()
        .jobs
        .iter()
        .find(|j| j.id == id)
        .map(|j| (j.shot.clone(), j.tag.clone()))
        .ok_or_else(|| plate_not_found(id))?;
    let sidecar = write_sidecar(&out_dir, &stem, &shot.0, shot.1.as_deref(), camera, profile, hash)?;
    let jobs = world.resource_mut::<PlateJobs>().into_inner();
    let job = jobs
        .jobs
        .iter_mut()
        .find(|j| j.id == id)
        .ok_or_else(|| plate_not_found(id))?;
    job.sidecar = Some(sidecar);
    let job = &*job;
    let sidecar = job.sidecar.as_ref().unwrap_or(&Value::Null);
    Ok(sidecar_answer(job, sidecar, "done"))
}

fn sidecar_answer(job: &PlateJob, sidecar: &Value, status: &str) -> Value {
    json!({
        "plate": job.id,
        "job": job.render_job,
        "status": status,
        "name": job.stem,
        "sidecar": sidecar,
    })
}

fn plate_not_found(id: u64) -> RpcError {
    reasoned(
        protocol::NOT_FOUND,
        "F0003",
        "plate_not_found",
        format!("no plate {id}"),
        "seeing.capture_plate returns the plate id; only the last 32 plates are kept",
    )
}

/// The camera and the profile preset as they stand, for a plate's sidecar.
fn rendered_state(world: &mut World, camera: Option<Entity>) -> (Value, Option<Value>) {
    let camera = camera_json(world, camera);
    let profile = crate::render_profile::get(world)
        .ok()
        .map(|p| p["preset"].clone());
    (camera, profile)
}

/// Called by render-out as it books delivered frame `index` of job `render_job`: a plate rendered
/// by that job keeps the camera and profile of its first frame for its sidecar (corr-agent
/// finding 15, ADR-5895). Before, the sidecar read them at the first poll after the job ended, so
/// a plate started in between moved the earlier plate's recorded camera.
pub(crate) fn frame_delivered(world: &mut World, render_job: u64, index: u64) {
    if index != 0 {
        return;
    }
    let Some(camera) = world.get_resource::<PlateJobs>().and_then(|jobs| {
        jobs.jobs
            .iter()
            .find(|j| j.render_job == render_job && j.rendered.is_none())
            .map(|j| j.camera)
    }) else {
        return;
    };
    let rendered = rendered_state(world, camera);
    if let Some(mut jobs) = world.get_resource_mut::<PlateJobs>()
        && let Some(job) = jobs
            .jobs
            .iter_mut()
            .find(|j| j.render_job == render_job && j.rendered.is_none())
    {
        job.rendered = Some(rendered);
    }
}

/// The pose of the camera the frame was rendered with, as the sidecar records it: the shot's
/// camera when it named or posed one, else the primary camera.
fn camera_json(world: &mut World, shot_camera: Option<Entity>) -> Value {
    let Some(entity) = shot_camera
        .filter(|e| world.get::<Camera>(*e).is_some())
        .or_else(|| pick_camera(world, None))
    else {
        return Value::Null;
    };
    let Some(camera) = world.get::<Camera>(entity) else {
        return Value::Null;
    };
    let fov_y_deg = camera.fov_y.to_degrees();
    let Some(global) = world.get::<GlobalTransform>(entity) else {
        return json!({ "fov_y_deg": fov_y_deg });
    };
    let (_, rotation, translation) = global.0.to_scale_rotation_translation();
    let forward = rotation * regia_core::math::Vec3::NEG_Z;
    json!({
        "eye": [translation.x, translation.y, translation.z],
        "forward": [forward.x, forward.y, forward.z],
        "fov_y_deg": fov_y_deg,
    })
}

fn shot_json(shot: &Shot) -> Value {
    json!({
        "name": shot.name,
        "level": shot.level,
        "camera": shot.camera.as_ref().map(|c| match c {
            ShotCamera::Named(name) => json!({ "name": name }),
            ShotCamera::Pose { eye, target, fov_y_deg } => json!({
                "eye": [eye.0, eye.1, eye.2],
                "target": [target.0, target.1, target.2],
                "fov_y_deg": fov_y_deg,
            }),
        }),
        "time_of_day": shot.time_of_day.as_ref().map(|t| json!({
            "hour": t.hour, "day_of_year": t.day_of_year, "latitude_deg": t.latitude_deg,
        })),
        "profile": shot.profile,
        "width": shot.width.unwrap_or(DEFAULT_SIZE.0),
        "height": shot.height.unwrap_or(DEFAULT_SIZE.1),
        "warmup_frames": shot.warmup_frames.unwrap_or(DEFAULT_WARMUP),
        "aovs": shot.aovs,
        "format": shot.format.as_deref().unwrap_or("png"),
    })
}

/// Every file path string in a manifest `files` value (frames, then the AOV files).
fn manifest_files(files: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(&files["frames"], &mut out);
    walk(&files["aov"], &mut out);
    out
}

/// Writes `<stem>.plate.json` next to the frame (a tmp file, then a rename) and returns it.
fn write_sidecar(
    out_dir: &Path,
    stem: &str,
    shot: &Shot,
    tag: Option<&str>,
    camera: Value,
    profile: Option<Value>,
    frame_hash: Value,
) -> Result<Value, RpcError> {
    let manifest_path = out_dir.join(format!("{stem}_render_out.json"));
    let manifest: Value = std::fs::read(&manifest_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| {
            reasoned(
                protocol::INTERNAL_ERROR,
                "F0001",
                "plate_failed",
                "the render-out manifest is missing or unreadable",
                "render.out_progress shows the job",
            )
        })?;
    let mut files = Vec::new();
    for shown in manifest_files(&manifest["files"]) {
        // Manifest paths are shown relative to the project root, or absolute; the file is next
        // to the manifest either way.
        let Some(file_name) = Path::new(&shown).file_name() else {
            continue;
        };
        let path = out_dir.join(file_name);
        let bytes = std::fs::read(&path).map_err(|e| {
            reasoned(
                protocol::INTERNAL_ERROR,
                "F0001",
                "plate_failed",
                format!("cannot read {}: {e}", file_name.to_string_lossy()),
                "render.out_progress shows the job",
            )
        })?;
        files.push(json!({
            "file": file_name.to_string_lossy(),
            "bytes": bytes.len(),
            "blake3": blake3::hash(&bytes).to_hex().to_string(),
        }));
    }
    let sidecar = json!({
        "schema": PLATE_SIDECAR_SCHEMA,
        "engine": env!("CARGO_PKG_VERSION"),
        "name": stem,
        "tag": tag,
        "shot": shot_json(shot),
        "profile": profile,
        "camera": camera,
        "frame_hash": frame_hash,
        "files": files,
    });
    let bytes = serde_json::to_vec_pretty(&sidecar).map_err(|e| RpcError::internal(e.to_string()))?;
    let path = out_dir.join(format!("{stem}.plate.json"));
    let part = out_dir.join(format!("{stem}.plate.json.part"));
    std::fs::write(&part, &bytes)
        .and_then(|()| std::fs::rename(&part, &path))
        .map_err(|e| {
            reasoned(
                protocol::INTERNAL_ERROR,
                "F0001",
                "plate_failed",
                format!("cannot write the sidecar: {e}"),
                "check the output directory",
            )
        })?;
    Ok(sidecar)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// corr-agent (ADR-5597): an optional shot key with a wrong value read as absent (the plate
    /// rendered the scene's own value and reported success), and a pose whose view direction
    /// cannot normalize (eye and target 1e-30 apart, or an f32-overflowing difference) wrote a
    /// non-unit camera rotation. Each shot here was accepted before.
    #[test]
    fn bad_optional_fields_and_degenerate_poses_are_refused() {
        let reason = |e: &RpcError| {
            e.data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        for shot in [
            json!({ "name": "a", "time_of_day": { "hour": 12, "day_of_year": 70000 } }),
            json!({ "name": "a", "time_of_day": { "hour": 12, "day_of_year": -1 } }),
            json!({ "name": "a", "time_of_day": { "hour": 12, "latitude_deg": "north" } }),
            json!({ "name": "a", "camera": { "eye": [0, 0, 0], "target": [0, 0, -1], "fov_y_deg": "wide" } }),
            json!({ "name": "a", "camera": { "eye": [0, 0, 0], "target": [1e-30, 0, 0] } }),
            json!({ "name": "a", "camera": { "eye": [3e38, 0, 0], "target": [-3e38, 0, 0] } }),
        ] {
            let error = shot_from_json(&shot).expect_err("refused");
            assert_eq!(reason(&error).as_deref(), Some("bad_shot"), "{shot}: {error:?}");
        }
        let fine = json!({ "name": "a", "camera": { "eye": [0, 1, 4], "target": [0, 1, 0], "fov_y_deg": 50 },
                           "time_of_day": { "hour": 9.5, "day_of_year": 172, "latitude_deg": 45 } });
        assert!(shot_from_json(&fine).is_ok());
    }

    /// corr-agent (ADR-5597): `shots_file` was read with `read_to_string` after the write
    /// resolver, which accepts any file type, so a FIFO planted at the path blocked the world
    /// thread forever (this test hung before the fix). It is now refused at once.
    #[cfg(unix)]
    #[test]
    fn a_shots_file_that_is_a_pipe_is_refused_without_blocking() {
        let root = regia_test_utils::unique_temp_dir("plates-fifo");
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let made = std::process::Command::new("mkfifo")
            .arg(root.join("s.shot.ron"))
            .status()
            .map(|s| s.success());
        assert_eq!(made.ok(), Some(true), "mkfifo");
        let error = resolve_shot(&world, &json!({ "shots_file": "s.shot.ron", "name": "a" }))
            .expect_err("a pipe is not a shot list");
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str),
            Some("shot_list_invalid"),
            "{error:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
