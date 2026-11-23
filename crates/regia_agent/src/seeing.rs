// SPDX-License-Identifier: MPL-2.0
//! Seeing (specs/23 § 3.4): screenshots, capture sequences, contact sheets, image comparison
//! and pixel picking.
//!
//! Camera-entity (`camera`) and UI-only (`ui`) targets and the debug overlays (colliders, light
//! radii, bounds, grid; [`crate::seeing_overlays`]) are served since ADR-7217. GPU picking needs a
//! picking buffer this build does not have; `seeing.pick_entity` answers a clear `F0013` instead
//! of pretending.

use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use regia_app::prelude::*;
use regia_platform::PrimaryWindow;
use regia_render::{CapturedImage, ScreenshotOptions, ScreenshotRequests};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

use crate::protocol::{self, Response, RpcError};

/// Reply outcome: answered now, or deferred (the handler keeps the reply channel).
pub(crate) enum Outcome {
    /// Answered within the handler.
    Done(Result<Value, RpcError>),
    /// The handler answers later (screenshots serve at the next present).
    Deferred,
}

/// EV100 the asset-preview camera pins: a sun-lit studio look with the
/// physically based pipeline (deterministic screenshots, no auto exposure).
const PREVIEW_EXPOSURE_EV: f32 = 14.5;

/// The asset preview stage: the entities `seeing.screenshot(target=asset)`
/// spawns (mesh + camera + key light). Replaced on every `set_asset_preview`.
#[derive(Resource, Default, Clone, Copy)]
pub(crate) struct AssetPreviewStage {
    /// Camera entity (moved by orbit captures).
    pub camera: Option<Entity>,
    /// Mesh entity.
    pub mesh: Option<Entity>,
}

/// `path` as a project-relative `assets/...` path with forward slashes (a Windows `\\` separator
/// must not defeat the `assets/` prefix check).
fn asset_preview_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    if path.starts_with("assets/") {
        path
    } else {
        format!("assets/{path}")
    }
}

/// Puts a mesh asset on the preview stage (target `"asset"` of `seeing.screenshot`).
///
/// Spawns/replaces the preview entities: a `MeshRenderer` at the origin, a
/// camera at `[0, 0.2, 2.5]` looking at the model, and a 100 klx key light —
/// the runtime render path's equivalent of the old preview scene.
pub(crate) fn set_asset_preview(world: &mut World, path: &str) -> Result<(), RpcError> {
    let norm = asset_preview_path(path);
    let Some(mut server) = world.get_resource_mut::<regia_asset::AssetServer>() else {
        return Err(RpcError::new(
            protocol::NOT_FOUND,
            "F0016",
            "no AssetServer in this app",
            "asset previews need regia_asset::AssetPlugin",
        ));
    };
    let handle = server.load_mesh_path(&norm).map_err(|e| {
        RpcError::new(
            protocol::NOT_FOUND,
            "F0016",
            format!("cannot load {norm}: {e}"),
            "call assets.list to see the known paths",
        )
    })?;
    let (mesh, material) = {
        let Some(mesh) = server.meshes().get(&handle).cloned() else {
            return Err(RpcError::internal("mesh did not load"));
        };
        (mesh, Arc::new(regia_render::CpuMaterial::default()))
    };
    let cpu = Arc::new(regia_render::CpuMesh {
        vertices: mesh
            .positions
            .iter()
            .zip(mesh.normals.iter())
            .zip(mesh.uvs.iter())
            .enumerate()
            .map(|(index, ((position, normal), uv))| regia_render::LabVertex {
                position: *position,
                normal: *normal,
                uv: *uv,
                tangent: mesh.tangents.get(index).copied().unwrap_or([1.0, 0.0, 0.0, 1.0]),
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect(),
        indices: mesh.indices.clone(),
    });

    let stage = *world.get_resource_or_insert_with(AssetPreviewStage::default);
    // Replace the mesh on the existing stage entity (stable GPU cache identity),
    // or spawn the stage the first time.
    match stage.mesh {
        Some(entity) => match world.get_mut::<regia_render::MeshRenderer>(entity) {
            Some(mut renderer) => {
                renderer.mesh = cpu;
                renderer.material = material;
            }
            None => return Err(RpcError::internal("preview mesh entity lost its renderer")),
        },
        None => {
            let transform = Transform::default();
            let mesh = world
                .spawn((
                    regia_scene::components::Name::new("agent_preview_mesh"),
                    transform,
                    GlobalTransform(transform.compute_affine()),
                    regia_render::MeshRenderer::new(cpu, material),
                ))
                .id();
            world.resource_mut::<AssetPreviewStage>().mesh = Some(mesh);
        }
    }
    if stage.camera.is_none() {
        // Camera at the old preview position, looking at the model's centre.
        let (camera, _light) = spawn_preview_camera_and_light(world);
        world.resource_mut::<AssetPreviewStage>().camera = Some(camera);
    }
    Ok(())
}

/// Spawns the preview camera (EV-pinned, deterministic) and key light.
fn spawn_preview_camera_and_light(world: &mut World) -> (Entity, Entity) {
    use regia_core::math::{Quat, Vec3};
    let eye = Vec3::new(0.0, 0.2, 2.5);
    let look = Vec3::ZERO;
    let forward = (look - eye).normalize_or_zero();
    let camera_transform = Transform {
        translation: eye,
        rotation: Quat::from_rotation_arc(-Vec3::Z, forward),
        scale: Vec3::ONE,
    };
    let camera = world
        .spawn((
            regia_scene::components::Name::new("agent_preview_camera"),
            camera_transform,
            GlobalTransform(camera_transform.compute_affine()),
            regia_render::Camera {
                exposure_ev: Some(PREVIEW_EXPOSURE_EV),
                ..regia_render::Camera::default()
            },
        ))
        .id();
    let dir = Vec3::new(-0.577, -0.577, -0.577).normalize_or_zero();
    let light_transform = Transform {
        translation: Vec3::ZERO,
        rotation: Quat::from_rotation_arc(-Vec3::Z, dir),
        scale: Vec3::ONE,
    };
    let light = world
        .spawn((
            regia_scene::components::Name::new("agent_preview_light"),
            light_transform,
            GlobalTransform(light_transform.compute_affine()),
            regia_render::DirectionalLight {
                direction: [-0.577, -0.577, -0.577],
                color: [1.0, 1.0, 1.0],
                illuminance: 100_000.0,
                cast_shadows: false,
                static_shadow: false,
            },
        ))
        .id();
    (camera, light)
}

/// Moves the preview camera to an orbit position (contact-sheet captures).
pub(crate) fn move_preview_camera(world: &mut World, eye: [f32; 3]) {
    use regia_core::math::{Quat, Vec3};
    let Some(stage) = world.get_resource::<AssetPreviewStage>() else {
        return;
    };
    let Some(camera) = stage.camera else { return };
    let Ok(mut entity) = world.get_entity_mut(camera) else {
        return;
    };
    let eye_v = Vec3::from_array(eye);
    let forward = (Vec3::ZERO - eye_v).normalize_or_zero();
    let transform = Transform {
        translation: eye_v,
        rotation: Quat::from_rotation_arc(-Vec3::Z, forward),
        scale: Vec3::ONE,
    };
    entity.insert(transform);
    entity.insert(GlobalTransform(transform.compute_affine()));
}

fn primary_window(world: &mut World) -> Result<Entity, RpcError> {
    let mut q = world.query_filtered::<Entity, With<PrimaryWindow>>();
    q.iter(world).next().ok_or_else(|| {
        RpcError::not_found(
            "no primary window yet",
            "wait for the app to resume (lifecycle Started)",
        )
    })
}

fn require_renderer(world: &mut World) -> Result<(), RpcError> {
    if world.get_resource::<ScreenshotRequests>().is_none() {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "no renderer in this app",
            "add regia_render::RenderPlugin",
        ));
    }
    Ok(())
}

/// The `calibration` parameter of `seeing.screenshot`: `"applied"` (default, what the player
/// sees) or `"neutral"` (before the display calibration: the image goldens compare).
fn screenshot_options(params: &Value) -> Result<ScreenshotOptions, RpcError> {
    match crate::params::opt_str(params, "calibration")? {
        None | Some("applied") => Ok(ScreenshotOptions { neutral: false }),
        Some("neutral") => Ok(ScreenshotOptions { neutral: true }),
        Some(other) => Err(RpcError::invalid_params(format!(
            "unknown calibration {other:?} (\"applied\" or \"neutral\")"
        ))),
    }
}

/// The `overlays` parameter of `seeing.screenshot` (ADR-7217): every name must be one of
/// [`crate::seeing_overlays::RUNTIME_OVERLAYS`]. `selection` and `labels` need the editor's state
/// (`F0013`, reason `overlay_needs_editor`); any other name is a parameter error.
fn overlay_names(params: &Value) -> Result<Vec<&'static str>, RpcError> {
    let Some(list) = crate::params::opt_array(params, "overlays")? else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for item in list {
        let Some(name) = item.as_str() else {
            return Err(RpcError::invalid_params(
                "`overlays` must be an array of overlay names",
            ));
        };
        if let Some(known) = crate::seeing_overlays::RUNTIME_OVERLAYS
            .iter()
            .find(|known| **known == name)
        {
            if !names.contains(known) {
                names.push(*known);
            }
        } else if crate::seeing_overlays::EDITOR_ONLY_OVERLAYS.contains(&name) {
            return Err(crate::jobs::reason_error(
                protocol::UNSUPPORTED,
                "F0013",
                "overlay_needs_editor",
                format!(
                    "overlay {name:?} draws editor state (the selection, entity labels): capture it with \
                     editor.capture_viewport after editor.set_viewport"
                ),
            ));
        } else {
            return Err(RpcError::invalid_params(format!(
                "unknown overlay {name:?} (colliders, light_radii, bounds, grid)"
            )));
        }
    }
    Ok(names)
}

/// The output size of an offscreen, camera or UI capture: `size: [w, h]`, else `width` and
/// `height`, each in 1..=4096.
fn capture_size(params: &Value, default: (u64, u64), target: &str) -> Result<(u32, u32), RpcError> {
    let bad = || {
        RpcError::invalid_params(format!(
            "`width`/`height` must be in 1..=4096 for target {target:?}"
        ))
    };
    let check = |value: u64| -> Result<u32, RpcError> {
        u32::try_from(value)
            .ok()
            .filter(|n| (1..=4096).contains(n))
            .ok_or_else(bad)
    };
    if let Some(size) = params.get("size").filter(|v| !v.is_null()) {
        let pair = size
            .as_array()
            .filter(|a| a.len() == 2)
            .and_then(|a| Some((a[0].as_u64()?, a[1].as_u64()?)))
            .ok_or_else(|| RpcError::invalid_params("`size` must be [width, height]"))?;
        return Ok((check(pair.0)?, check(pair.1)?));
    }
    let side = |key: &str, default: u64| -> Result<u32, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => check(default),
            Some(v) => check(v.as_u64().unwrap_or(0)),
        }
    };
    Ok((side("width", default.0)?, side("height", default.1)?))
}

/// `seeing.screenshot`: window (default), asset preview, headless offscreen, one camera entity
/// (`camera`) or the UI layer alone (`ui`), with optional debug overlays (ADR-7217).
pub(crate) fn screenshot(world: &mut World, params: &Value, id: Value, reply: Sender<Response>) -> Outcome {
    let target = match crate::params::opt_str(params, "target") {
        Ok(t) => t.unwrap_or("window"),
        Err(error) => return Outcome::Done(Err(error)),
    };
    let options = match screenshot_options(params) {
        Ok(options) => options,
        Err(error) => return Outcome::Done(Err(error)),
    };
    let overlays = match overlay_names(params) {
        Ok(names) => names,
        Err(error) => return Outcome::Done(Err(error)),
    };
    let view_mode = match crate::params::opt_str(params, "view_mode") {
        Ok(v) => v,
        Err(error) => return Outcome::Done(Err(error)),
    };
    if let Some(view_mode) = view_mode {
        return Outcome::Done(Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0013",
            format!("view mode {view_mode:?} is not available"),
            "this build renders the lit forward pass only",
        )));
    }
    match target {
        "offscreen" => return Outcome::Done(screenshot_offscreen(world, params, options, &overlays)),
        "camera" => return Outcome::Done(screenshot_camera(world, params, options, &overlays)),
        "ui" => return Outcome::Done(screenshot_ui(world, params, options)),
        "asset" => {
            let path = match crate::params::opt_str(params, "path") {
                Ok(p) => p,
                Err(error) => return Outcome::Done(Err(error)),
            };
            let Some(path) = path else {
                return Outcome::Done(Err(RpcError::invalid_params(
                    "`path` (project-relative under assets/) is required for target \"asset\"",
                )));
            };
            if let Err(error) = set_asset_preview(world, path) {
                return Outcome::Done(Err(error));
            }
        }
        "window" => {}
        other => {
            return Outcome::Done(Err(RpcError::invalid_params(format!(
                "unknown target {other:?} (window, asset, offscreen, camera, ui)"
            ))));
        }
    }
    if let Err(error) = require_renderer(world) {
        return Outcome::Done(Err(error));
    }
    let entity_bits = match crate::params::opt_u64(params, "entity") {
        Ok(b) => b,
        Err(error) => return Outcome::Done(Err(error)),
    };
    let window = match entity_bits {
        Some(bits) => Entity::try_from_bits(bits),
        None => primary_window(world).ok(),
    };
    // Overlays are projected through the primary view as extracted now; the picture is the
    // next present's (one frame later at most).
    let overlay = if overlays.is_empty() {
        None
    } else {
        let view = world
            .get_resource::<regia_render::runtime::ExtractedFrame>()
            .and_then(regia_render::runtime::ExtractedFrame::primary_view)
            .map(|view| crate::seeing_overlays::OverlayView::of(&view.camera));
        match view {
            Some(view) => Some((view, crate::seeing_overlays::segments_for(world, &overlays))),
            None => return Outcome::Done(Err(no_view_for_overlays())),
        }
    };
    let Some(mut requests) = world.get_resource_mut::<ScreenshotRequests>() else {
        return Outcome::Done(Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "no renderer in this app",
            "add regia_render::RenderPlugin",
        )));
    };
    requests.request_with(
        window,
        options,
        Box::new(move |result| {
            // The caller already got F0006: skip the PNG encode nobody reads.
            if crate::server::take_abandoned(&reply) {
                return;
            }
            let result = result.map(|mut img| {
                if let Some((view, segments)) = &overlay {
                    crate::seeing_overlays::draw_segments(
                        &mut img.rgba,
                        img.width,
                        img.height,
                        view,
                        segments,
                    );
                }
                img
            });
            let response = match result.and_then(|img| crate::image::png_base64(&img).map(|b64| (img, b64))) {
                Ok((img, b64)) => {
                    let (width, height) = (img.width, img.height);
                    // The pixels go back to the capture pool for the next shot (ADR-8848).
                    regia_render::screenshot::recycle(img);
                    Response::ok(id, png_reply(width, height, b64))
                }
                Err(e) => Response::err(id, RpcError::internal(format!("screenshot failed: {e}"))),
            };
            let _ = reply.send(response);
        }),
    );
    Outcome::Deferred
}

/// `{width, height, mime, png_base64}` with the base64 string moved in, not copied: `json!`
/// serializes an expression by reference, which cloned the reply-sized text (ADR-8848).
fn png_reply(width: u32, height: u32, png_base64: String) -> Value {
    let mut fields = serde_json::Map::new();
    fields.insert("width".into(), Value::from(width));
    fields.insert("height".into(), Value::from(height));
    fields.insert("mime".into(), Value::from("image/png"));
    fields.insert("png_base64".into(), Value::String(png_base64));
    Value::Object(fields)
}

/// The refusal for overlays when no camera view was extracted yet.
fn no_view_for_overlays() -> RpcError {
    crate::jobs::reason_error(
        protocol::NOT_FOUND,
        "F0003",
        "screenshot_no_view",
        "overlays need a camera view to project through, and no camera was extracted yet: \
         spawn a Camera and step one frame (play.step)",
    )
}

/// Applies the display calibration (unless `neutral`), draws `overlays` through `view`, and
/// answers the PNG reply.
fn finish_capture(
    world: &World,
    mut img: CapturedImage,
    options: ScreenshotOptions,
    overlays: Option<OverlayPlan>,
    extra: Value,
) -> Result<Value, RpcError> {
    if !options.neutral
        && let Some(calibration) = world.get_resource::<regia_render::DisplayCalibration>()
    {
        calibration.apply_sdr_rgba8(&mut img.rgba);
    }
    let mut overlay_pixels = None;
    if let Some((view, segments)) = &overlays {
        overlay_pixels = Some(crate::seeing_overlays::draw_segments(
            &mut img.rgba,
            img.width,
            img.height,
            view,
            segments,
        ));
    }
    let b64 = crate::image::png_base64(&img).map_err(RpcError::internal)?;
    let (width, height) = (img.width, img.height);
    regia_render::screenshot::recycle(img);
    let mut out = png_reply(width, height, b64);
    if let Some(pixels) = overlay_pixels {
        out["overlay_pixels"] = json!(pixels);
    }
    if let (Some(object), Value::Object(extra)) = (out.as_object_mut(), extra) {
        object.extend(extra);
    }
    Ok(out)
}

/// Headless offscreen capture: no window, no compositor. Renders the extracted ECS
/// frame ([`regia_render::Camera`] + [`regia_render::MeshRenderer`]) through the
/// persistent [`regia_render::runtime::RuntimeRenderer`] and answers immediately.
///
/// Offscreen frames have no display stage: the applied capture runs the SDR calibration on
/// the CPU ([`regia_render::DisplayCalibration::apply_sdr_rgba8`], the same curve).
fn screenshot_offscreen(
    world: &mut World,
    params: &Value,
    options: ScreenshotOptions,
    overlays: &[&str],
) -> Result<Value, RpcError> {
    let (width, height) = capture_size(params, (256, 144), "offscreen")?;
    let window_layers = crate::params::opt_bool(params, "window_layers")?.unwrap_or(false);
    // The game HUD is part of a render-out: laid out for this output size and orientation.
    let hud = crate::params::opt_bool(params, "hud")?.unwrap_or(true);
    let (pixels, view) = render_with_hud(world, params, width, height, window_layers, hud, None)?;
    let overlays = overlay_segments(world, overlays, view)?;
    let img = CapturedImage {
        width,
        height,
        rgba: pixels,
    };
    finish_capture(world, img, options, overlays, Value::Null)
}

/// The view overlays are projected through and their world-space segments.
type OverlayPlan = (
    crate::seeing_overlays::OverlayView,
    Vec<crate::seeing_overlays::OverlaySegment>,
);

/// The overlay segments and the view to draw them through, or `None` without overlays.
fn overlay_segments(
    world: &mut World,
    overlays: &[&str],
    view: Option<crate::seeing_overlays::OverlayView>,
) -> Result<Option<OverlayPlan>, RpcError> {
    if overlays.is_empty() {
        return Ok(None);
    }
    let view = view.ok_or_else(no_view_for_overlays)?;
    Ok(Some((
        view,
        crate::seeing_overlays::segments_for(world, overlays),
    )))
}

/// Lays the HUD out for the output (when `hud` and the app has a [`regia_render::RenderOutHud`]),
/// renders the extracted view `view_index` (`None`: the primary window view) offscreen, and
/// answers the pixels with the view they were rendered from.
fn render_with_hud(
    world: &mut World,
    params: &Value,
    width: u32,
    height: u32,
    window_layers: bool,
    hud: bool,
    view_index: Option<usize>,
) -> Result<(Vec<u8>, Option<crate::seeing_overlays::OverlayView>), RpcError> {
    let layout = hud_layout(params)?;
    let out_spec = regia_render::RenderOutSpec::new(width, height, layout);
    let hud_hook = hud
        .then(|| world.get_resource::<regia_render::RenderOutHud>().cloned())
        .flatten();
    if let Some(hook) = &hud_hook {
        hook.begin(world, &out_spec);
    }
    let pixels = offscreen_render(
        world,
        width,
        height,
        window_layers,
        hud_hook.is_some(),
        view_index,
    );
    if let Some(hook) = &hud_hook {
        hook.end(world, &out_spec);
    }
    pixels
}

/// The `layout` parameter: the HUD layout of an offscreen or UI capture.
fn hud_layout(params: &Value) -> Result<regia_render::HudLayout, RpcError> {
    match crate::params::opt_str(params, "layout")? {
        None => Ok(regia_render::HudLayout::Auto),
        Some(name) => regia_render::HudLayout::parse(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown layout {name:?} (\"auto\", \"landscape\" or \"portrait\")"
            ))
        }),
    }
}

/// The camera entity a `camera` target names: `entity` (its bits) or `name` (its
/// [`regia_scene::components::Name`]; the first camera of that name in spawn order).
fn camera_entity(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    if let Some(bits) = crate::params::opt_u64(params, "entity")? {
        let entity = Entity::try_from_bits(bits)
            .filter(|entity| world.get_entity(*entity).is_ok())
            .ok_or_else(|| {
                RpcError::not_found(
                    format!("no entity {bits}"),
                    "find cameras with scene.query_entities (component Camera)",
                )
            })?;
        if world.get::<regia_render::Camera>(entity).is_none() {
            return Err(crate::jobs::reason_error(
                protocol::INVALID_PARAMS,
                "F0002",
                "screenshot_not_a_camera",
                format!("entity {bits} has no Camera component"),
            ));
        }
        return Ok(entity);
    }
    let Some(name) = crate::params::opt_str(params, "name")? else {
        return Err(RpcError::invalid_params(
            "target \"camera\" needs `entity` (a camera entity) or `name` (its Name)",
        ));
    };
    let mut cameras =
        world.query_filtered::<(Entity, &regia_scene::components::Name), With<regia_render::Camera>>();
    cameras
        .iter(world)
        .filter(|(_, n)| n.as_str() == name)
        .map(|(entity, _)| entity)
        .min_by_key(|entity| entity.index_u32())
        .ok_or_else(|| {
            RpcError::not_found(
                format!("no camera named {name:?}"),
                "find cameras with scene.query_entities (component Camera)",
            )
        })
}

/// `target: "camera"`: renders one camera entity's view offscreen (ADR-7217), whatever its
/// render target or viewport, at `width`×`height` (default 256×144). The HUD is off unless
/// `hud: true` (a second camera is not the player's screen).
fn screenshot_camera(
    world: &mut World,
    params: &Value,
    options: ScreenshotOptions,
    overlays: &[&str],
) -> Result<Value, RpcError> {
    let camera = camera_entity(world, params)?;
    let (width, height) = capture_size(params, (256, 144), "camera")?;
    let index = world
        .get_resource::<regia_render::runtime::ExtractedFrame>()
        .and_then(|frame| frame.view_index_of(camera))
        .ok_or_else(|| {
            crate::jobs::reason_error(
                protocol::NOT_FOUND,
                "F0003",
                "screenshot_camera_not_rendered",
                "the camera has no view in the extracted frame yet (spawned this frame, or no \
                 forward direction): step one frame (play.step) and try again",
            )
        })?;
    let window_layers = crate::params::opt_bool(params, "window_layers")?.unwrap_or(false);
    let hud = crate::params::opt_bool(params, "hud")?.unwrap_or(false);
    let (pixels, view) = render_with_hud(world, params, width, height, window_layers, hud, Some(index))?;
    let overlays = overlay_segments(world, overlays, view)?;
    let img = CapturedImage {
        width,
        height,
        rgba: pixels,
    };
    finish_capture(
        world,
        img,
        options,
        overlays,
        json!({ "camera": camera.to_bits() }),
    )
}

/// `target: "ui"`: the runtime UI layer alone (window overlay layer `"ui"`), laid out for
/// `width`×`height` (default 1280×720) and `layout`, over transparent black with no scene under
/// it (ADR-7217). Safe-area previews are the editor's (`editor.ui_designer_*`).
fn screenshot_ui(world: &mut World, params: &Value, options: ScreenshotOptions) -> Result<Value, RpcError> {
    let (width, height) = capture_size(params, (1280, 720), "ui")?;
    let has_ui = world
        .get_resource::<regia_render::WindowOverlay>()
        .is_some_and(|overlay| overlay.layer_names().contains(&"ui"));
    if !has_ui {
        return Err(crate::jobs::reason_error(
            protocol::UNSUPPORTED,
            "F0013",
            "screenshot_no_ui_layer",
            "this app draws no runtime UI layer (\"ui\"): add regia_ui's plugin, or capture the \
             window (target \"window\")",
        ));
    }
    ensure_device(world)?;
    let layout = hud_layout(params)?;
    let out_spec = regia_render::RenderOutSpec::new(width, height, layout);
    let hud_hook = world.get_resource::<regia_render::RenderOutHud>().cloned();
    if let Some(hook) = &hud_hook {
        hook.begin(world, &out_spec);
    }
    let pixels = world.resource_scope(
        |world, mut runtime: Mut<regia_render::runtime::RuntimeRenderer>| {
            let device = world.resource::<regia_render::RenderDevice>();
            let Some(overlay) = world.get_resource::<regia_render::WindowOverlay>() else {
                return Err(RpcError::internal("the window overlay went away"));
            };
            runtime
                .0
                .render_layers_only(device, width, height, overlay, [0.0; 4], |name| name == "ui")
                .map(|frame| frame.pixels)
                .map_err(|e| RpcError::internal(format!("UI render failed: {e}")))
        },
    );
    if let Some(hook) = &hud_hook {
        hook.end(world, &out_spec);
    }
    let img = CapturedImage {
        width,
        height,
        rgba: pixels?,
    };
    finish_capture(world, img, options, None, Value::Null)
}

/// Opens the headless device when the app has none (unless the host inserted [`GpuDisabled`])
/// and checks for the [`regia_render::runtime::RuntimeRenderer`].
fn ensure_device(world: &mut World) -> Result<(), RpcError> {
    if world.get_resource::<regia_render::RenderDevice>().is_none() {
        // A host that opened no GPU on purpose keeps it that way (ADR-5180).
        if let Some(disabled) = world.get_resource::<GpuDisabled>() {
            return Err(disabled.error());
        }
        match headless_render_device() {
            Ok(device) => {
                world.insert_resource(device);
            }
            Err(e) => {
                return Err(RpcError::new(
                    protocol::UNSUPPORTED,
                    "F0011",
                    format!("no GPU for offscreen capture: {e}"),
                    "install lavapipe (Vulkan software) or a GPU driver",
                ));
            }
        }
    }
    if world
        .get_resource::<regia_render::runtime::RuntimeRenderer>()
        .is_none()
    {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "no RuntimeRenderer",
            "add regia_render::RenderPlugin { headless: true }",
        ));
    }
    Ok(())
}

/// A host's decision that this process opens no GPU device (the headless editor's `--no-gpu`,
/// ADR-5180). While it is present, offscreen captures and render-out answer its error instead of
/// creating a headless device.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct GpuDisabled {
    /// The `data.code` of the refusal (lowercase ascii, digits and `_`).
    pub code: String,
    /// Why there is no GPU (the error message).
    pub reason: String,
    /// What to do about it (the error hint).
    pub hint: String,
}

impl GpuDisabled {
    /// The refusal a capture or a render-out answers.
    #[must_use]
    pub fn error(&self) -> RpcError {
        RpcError::new(
            protocol::UNSUPPORTED,
            &self.code,
            self.reason.clone(),
            self.hint.clone(),
        )
    }

    /// The host's refusal when it inserted `GpuDisabled`, before a method opens a device of its own.
    pub(crate) fn refuse(world: &World) -> Result<(), RpcError> {
        world
            .get_resource::<Self>()
            .map_or(Ok(()), |disabled| Err(disabled.error()))
    }
}

/// The engine device an agent method opens when the app has none (a capture, a render-out, a
/// reference trace), and the device this crate's own GPU tests draw on.
///
/// The shipped crate opens one per call, as before. This crate's unit-test binary shares one per
/// process instead (ADR-7531, the ADR-9602 pattern): each test opening a device of its own and
/// dropping it with its world destroyed devices while other test threads named objects, which is
/// the Vulkan loader list race (ADR-2113). The Intel Arc pass died in that walk
/// (`libvulkan.so.1.4.309` +0x2d066) at a different test each run.
///
/// # Errors
///
/// No adapter, or the device request failed ([`regia_render::RenderDevice::headless`]).
pub(crate) fn headless_render_device() -> Result<regia_render::RenderDevice, regia_render::RenderError> {
    #[cfg(test)]
    {
        regia_render::RenderDevice::shared_headless()
    }
    #[cfg(not(test))]
    {
        regia_render::RenderDevice::headless()
    }
}

/// The product viewer's own device (`viewer.*` and `looks.*`, `regia_viewer::gpu::headless_device`):
/// one per call in the shipped crate, one per process in this crate's unit tests, for the reason
/// `headless_render_device` gives (ADR-7531). A failed open is not kept; the next call tries again.
///
/// # Errors
///
/// [`regia_viewer::ViewerError::NoAdapter`] or [`regia_viewer::ViewerError::Device`], as the viewer
/// reports them.
pub(crate) fn viewer_device() -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue), regia_viewer::ViewerError>
{
    #[cfg(test)]
    {
        type Shared = (wgpu::Adapter, wgpu::Device, wgpu::Queue);
        static SHARED: Mutex<Option<Shared>> = Mutex::new(None);
        // A poisoned lock only means a test panicked while holding it; the slot is still valid.
        let mut slot = SHARED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(shared) = slot.as_ref() {
            return Ok(shared.clone());
        }
        let opened = regia_viewer::gpu::headless_device()?;
        *slot = Some(opened.clone());
        Ok(opened)
    }
    #[cfg(not(test))]
    {
        regia_viewer::gpu::headless_device()
    }
}

/// Keeps a device a unit test of this crate opened for itself (a minimum-spec profile) alive
/// until the process exits, so it is not destroyed while other test threads use the GPU
/// (ADR-7531, the loader list race of ADR-2113).
#[cfg(test)]
pub(crate) fn keep_until_exit(device: &regia_render::RenderDevice) {
    static KEPT: Mutex<Vec<regia_render::RenderDevice>> = Mutex::new(Vec::new());
    KEPT.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(device.clone());
}

/// Renders the extracted frame offscreen at `width`×`height` and returns tightly packed RGBA8
/// (no display calibration). `window_layers` draws every window overlay layer, `ui` only the
/// game UI; the caller has already laid the UI out for this size ([`regia_render::RenderOutHud`]).
/// Creates the headless device when the app has none (lavapipe in CI and agent runs), unless the
/// host inserted [`GpuDisabled`].
pub(crate) fn offscreen_pixels(
    world: &mut World,
    width: u32,
    height: u32,
    window_layers: bool,
    ui: bool,
) -> Result<Vec<u8>, RpcError> {
    offscreen_render(world, width, height, window_layers, ui, None).map(|(pixels, _)| pixels)
}

/// [`offscreen_pixels`] for the extracted view `view_index` (`None`: the primary window view),
/// answering the view the pixels were rendered from (for overlays).
fn offscreen_render(
    world: &mut World,
    width: u32,
    height: u32,
    window_layers: bool,
    ui: bool,
    view_index: Option<usize>,
) -> Result<(Vec<u8>, Option<crate::seeing_overlays::OverlayView>), RpcError> {
    ensure_device(world)?;
    world.resource_scope(
        |world, mut runtime: Mut<regia_render::runtime::RuntimeRenderer>| {
            let device = world.resource::<regia_render::RenderDevice>();
            let Some(extracted) = world.get_resource::<regia_render::runtime::ExtractedFrame>() else {
                return Err(RpcError::new(
                    protocol::UNSUPPORTED,
                    "F0011",
                    "nothing to capture offscreen",
                    "spawn a Camera and a MeshRenderer; offscreen capture renders the extracted frame",
                ));
            };
            if !extracted.is_renderable() {
                return Err(RpcError::new(
                    protocol::UNSUPPORTED,
                    "F0011",
                    "nothing to capture offscreen",
                    "spawn a Camera and a MeshRenderer; offscreen capture renders the extracted frame",
                ));
            }
            // `window_views` returns owned data, so the borrow ends before `frame_scene`.
            let view_index = view_index.unwrap_or_else(|| {
                extracted
                    .window_views()
                    .first()
                    .map(|(index, _)| *index)
                    .unwrap_or(0)
            });
            let view = extracted
                .views
                .get(view_index)
                .map(|view| crate::seeing_overlays::OverlayView::of(&view.camera));
            runtime.0.set_glass(extracted.glass.clone());
            // The valley mist rides the extracted frame like the glass: a headless app draws only
            // through this capture, so without it the mist never reaches a pixel (ADR-1453).
            runtime.0.set_valley_mist(extracted.valley_mist.clone());
            runtime.0.set_mask_decals(&extracted.mask_decals);
            runtime.0.set_morph_instances(&extracted.morphs);
            let scene = extracted.frame_scene(view_index, width, height);
            // A visible shot fade is part of the picture: render-out and plain captures draw it
            // too (ADR-1522), after the UI.
            let fading = world
                .get_resource::<regia_framework::shots::ShotFadeLayer>()
                .is_some_and(|layer| layer.0.get().is_visible());
            let layers = (window_layers || ui || fading)
                .then(|| world.get_resource::<regia_render::WindowOverlay>())
                .flatten();
            match layers {
                Some(overlay) if window_layers => {
                    runtime.0.render_offscreen_with_overlay(device, &scene, overlay)
                }
                Some(overlay) => runtime
                    .0
                    .render_offscreen_with_layers(device, &scene, overlay, |name| {
                        (ui && name == "ui") || (fading && name == regia_render::FADE_LAYER)
                    }),
                None => runtime.0.render_offscreen(device, &scene),
            }
            .map(|frame| (frame.pixels, view))
            .map_err(|e| RpcError::internal(format!("offscreen render failed: {e}")))
        },
    )
}

/// `seeing.pick_entity`: the entity under a window pixel. Needs a GPU picking buffer, which
/// the renderer does not expose yet — an honest `F0013` until the renderer lane adds one.
pub(crate) fn pick_entity(params: &Value) -> Result<Value, RpcError> {
    let _ = params;
    Err(RpcError::new(
        protocol::UNSUPPORTED,
        "F0013",
        "pixel picking needs the renderer's entity-id buffer",
        "identify entities spatially with scene.query_entities (near/box) instead",
    ))
}

/// `seeing.memory_report`: the renderer's GPU memory by category (every live allocation of
/// the device, sorted by its wgpu label, [`regia_render::memory_report`]), the process RSS
/// and its peak, and the upload staging flush ([`regia_render::staging_flush_stats`]).
/// `top` (default 20, max 200) limits the per-label list.
pub(crate) fn memory_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let top = crate::params::opt_u64(params, "top")?.unwrap_or(20).min(200) as usize;
    let Some(device) = world.get_resource::<regia_render::RenderDevice>() else {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "no render device yet",
            "open a window or take an offscreen screenshot first (seeing.screenshot target=offscreen)",
        ));
    };
    let mut out = memory_report_json(&regia_render::memory_report(device), top);
    add_load_memory(
        &mut out,
        regia_render::process_peak_rss_bytes(),
        &regia_render::staging_flush_stats(),
    );
    Ok(out)
}

/// Adds the peak RSS and the staging flush (ADR-1346) to a `seeing.memory_report` result.
fn add_load_memory(out: &mut Value, peak_rss_bytes: Option<u64>, staging: &regia_render::StagingFlushStats) {
    out["peak_rss_bytes"] = json!(peak_rss_bytes);
    out["staging"] = json!({
        "cap_bytes": staging.cap_bytes,
        "flushes": staging.flushes,
        "flushed_bytes": staging.flushed_bytes,
        "waits": staging.waits,
        "peak_pending_bytes": staging.peak_pending_bytes,
    });
}

/// The JSON shape of a [`regia_render::MemoryReport`].
fn memory_report_json(report: &regia_render::MemoryReport, top: usize) -> Value {
    let categories: serde_json::Map<String, Value> = report
        .categories
        .iter()
        .map(|(category, total)| {
            (
                category.name().to_owned(),
                json!({ "bytes": total.bytes, "count": total.count }),
            )
        })
        .collect();
    let labels: Vec<Value> = report
        .labels
        .iter()
        .take(top)
        .map(|l| json!({ "label": l.label, "category": l.category.name(), "bytes": l.bytes, "count": l.count }))
        .collect();
    json!({
        "available": report.available,
        "backend": report.backend,
        "allocated_bytes": report.allocated_bytes,
        "reserved_bytes": report.reserved_bytes,
        "rss_bytes": report.rss_bytes,
        "device_allocated_bytes": report.device_allocated_bytes,
        "categories": categories,
        "labels": labels,
    })
}

/// What a multi-capture job is shooting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureKind {
    /// N window frames, `every_n` frames apart.
    Sequence,
    /// One mesh asset from N orbit angles.
    Orbit,
    /// N window frames fed to a photosensitivity check. Pixels are not kept.
    Flash,
}

/// What the driver should do next for one capture job.
#[derive(Debug)]
enum CaptureAction {
    /// Request a screenshot now (orbit jobs move the preview camera first).
    Request { orbit: Option<[f32; 3]> },
    /// Nothing to do this frame.
    Wait,
    /// The job is done (strip composed, or it failed).
    Finished(Result<Value, RpcError>),
}

/// One in-flight capture job (deferred reply).
struct Capture {
    /// JSON-RPC id to answer with.
    id: Value,
    /// Reply channel (dropped after the timeout server-side).
    reply: Sender<Response>,
    /// What is being captured.
    kind: CaptureKind,
    /// Total screenshots to take.
    total: usize,
    /// Frames between captures.
    every_n: u64,
    /// Next frame a capture may be requested in.
    next_due_frame: u64,
    /// Orbit positions (kind Orbit), one per capture, already including the first.
    orbit: Vec<[f32; 3]>,
    /// True while a screenshot of this job is outstanding.
    in_flight: bool,
    /// Completed captures. Empty for a flash job, which does not keep pixels.
    images: Vec<CapturedImage>,
    /// Screenshots a flash job has accepted.
    received: usize,
    /// Analyzer for [`CaptureKind::Flash`]. `None` for a sequence or an orbit.
    flash: Option<crate::seeing_flash::FlashJob>,
}

impl Capture {
    /// Feeds the results the render thread delivered since last frame and decides the next
    /// action. Pure: the driver performs the world interaction.
    fn advance(&mut self, frame: u64, delivered: Vec<Result<CapturedImage, String>>) -> CaptureAction {
        for result in delivered {
            self.in_flight = false;
            match result {
                Ok(image) => {
                    if let Some(job) = self.flash.as_mut() {
                        if let Err(error) = job.push(&image) {
                            return CaptureAction::Finished(Err(error));
                        }
                        self.received = self.received.saturating_add(1);
                    } else {
                        self.images.push(image);
                    }
                }
                Err(error) => {
                    return CaptureAction::Finished(Err(RpcError::new(
                        protocol::UNSUPPORTED,
                        "F0015",
                        format!("capture failed: {error}"),
                        "check logs.read; the window may have closed",
                    )));
                }
            }
        }
        let complete = if self.flash.is_some() {
            self.received >= self.total
        } else {
            self.images.len() >= self.total
        };
        if !self.in_flight && complete {
            if let Some(job) = self.flash.take() {
                return CaptureAction::Finished(job.finish());
            }
            let images = std::mem::take(&mut self.images);
            return CaptureAction::Finished(compose_answer(self.kind, &images));
        }
        if !self.in_flight && frame >= self.next_due_frame {
            let orbit = if self.kind == CaptureKind::Orbit {
                self.orbit.get(self.images.len()).copied()
            } else {
                None
            };
            self.in_flight = true;
            self.next_due_frame = frame.saturating_add(self.every_n);
            return CaptureAction::Request { orbit };
        }
        CaptureAction::Wait
    }
}

/// A completed screenshot, or why it failed — shared between the render thread and the driver.
type Delivery = Result<CapturedImage, String>;

/// The active multi-capture jobs, driven by [`advance_captures`].
#[derive(Resource, Default)]
pub struct CaptureQueue {
    captures: Vec<(Capture, Arc<Mutex<Vec<Delivery>>>)>,
}

/// Most multi-frame capture jobs (`capture_sequence`, `contact_sheet`) running at once (ADR-1034).
const MAX_CAPTURE_JOBS: usize = 8;

/// The busy answer of a deferred method whose pending list is full: `too_many_pending`, so a
/// flood of calls (notifications included, which never wait for a reply) cannot grow the list.
pub(crate) fn too_many_pending(what: &str, max: usize) -> RpcError {
    RpcError::new(
        protocol::INVALID_REQUEST,
        "too_many_pending",
        format!("{max} {what} calls are already pending"),
        "wait for the pending calls to answer, then try again",
    )
}

/// Starts a capture job. Shared by `capture_sequence` and `contact_sheet`.
fn start_capture(
    world: &mut World,
    id: Value,
    reply: Sender<Response>,
    kind: CaptureKind,
    total: usize,
    every_n: u64,
    orbit: Vec<[f32; 3]>,
) -> Outcome {
    if let Err(error) = require_renderer(world) {
        return Outcome::Done(Err(error));
    }
    if primary_window(world).is_err() {
        return Outcome::Done(Err(RpcError::not_found(
            "no primary window yet",
            "wait for the app to resume (lifecycle Started)",
        )));
    }
    let running = world
        .get_resource::<CaptureQueue>()
        .map_or(0, |queue| queue.captures.len());
    if running >= MAX_CAPTURE_JOBS {
        return Outcome::Done(Err(too_many_pending("multi-frame capture", MAX_CAPTURE_JOBS)));
    }
    let frame = world.resource::<Time>().frame_count();
    world
        .get_resource_or_insert_with(CaptureQueue::default)
        .captures
        .push((
            Capture {
                id,
                reply,
                kind,
                total,
                every_n,
                next_due_frame: frame,
                orbit,
                in_flight: false,
                images: Vec::new(),
                received: 0,
                flash: None,
            },
            Arc::new(Mutex::new(Vec::new())),
        ));
    Outcome::Deferred
}

/// Largest `every_n` a `seeing.capture_sequence` accepts (frames between captures).
const MAX_EVERY_N: u64 = 3600;

/// Parses and range-checks `every_n` (default 5, `1..=MAX_EVERY_N`).
fn capture_every_n(params: &Value) -> Result<u64, RpcError> {
    match params.get("every_n") {
        None | Some(Value::Null) => Ok(5),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_EVERY_N).contains(n))
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`every_n` must be an integer in 1..={MAX_EVERY_N}"))
            }),
    }
}

/// `seeing.capture_sequence`: `frames` window screenshots, `every_n` frames apart, as a strip.
pub(crate) fn capture_sequence(
    world: &mut World,
    params: &Value,
    id: Value,
    reply: Sender<Response>,
) -> Outcome {
    let frames = match crate::params::opt_u64(params, "frames") {
        Ok(f) => f.unwrap_or(4).clamp(1, 32) as usize,
        Err(error) => return Outcome::Done(Err(error)),
    };
    let every_n = match capture_every_n(params) {
        Ok(n) => n,
        Err(error) => return Outcome::Done(Err(error)),
    };
    start_capture(
        world,
        id,
        reply,
        CaptureKind::Sequence,
        frames,
        every_n,
        Vec::new(),
    )
}

/// `seeing.contact_sheet`: a mesh asset from `angles` orbit positions, as a strip.
pub(crate) fn contact_sheet(
    world: &mut World,
    params: &Value,
    id: Value,
    reply: Sender<Response>,
) -> Outcome {
    let path = match crate::params::opt_str(params, "path") {
        Ok(p) => p,
        Err(error) => return Outcome::Done(Err(error)),
    };
    let Some(path) = path else {
        return Outcome::Done(Err(RpcError::invalid_params(
            "`path` (a mesh asset path) is required",
        )));
    };
    let angles = match crate::params::opt_u64(params, "angles") {
        Ok(a) => a.unwrap_or(4).clamp(1, 8) as usize,
        Err(error) => return Outcome::Done(Err(error)),
    };
    if let Err(error) = set_asset_preview(world, path) {
        return Outcome::Done(Err(error));
    }
    let radius = 2.5_f32;
    let orbit: Vec<[f32; 3]> = (0..angles)
        .map(|i| {
            let theta = i as f32 * std::f32::consts::TAU / angles as f32;
            [radius * theta.sin(), 0.2, radius * theta.cos()]
        })
        .collect();
    start_capture(world, id, reply, CaptureKind::Orbit, angles, 1, orbit)
}

/// Starts a flash-check capture. The job analyzes each frame and drops the pixels.
pub(crate) fn start_flash_capture(
    world: &mut World,
    id: Value,
    reply: Sender<Response>,
    total: usize,
    every_n: u64,
    job: crate::seeing_flash::FlashJob,
) -> Outcome {
    if let Err(error) = require_renderer(world) {
        return Outcome::Done(Err(error));
    }
    if primary_window(world).is_err() {
        return Outcome::Done(Err(RpcError::not_found(
            "no primary window yet",
            "wait for the app to resume (lifecycle Started)",
        )));
    }
    let running = world
        .get_resource::<CaptureQueue>()
        .map_or(0, |queue| queue.captures.len());
    if running >= MAX_CAPTURE_JOBS {
        return Outcome::Done(Err(too_many_pending("multi-frame capture", MAX_CAPTURE_JOBS)));
    }
    let frame = world.resource::<Time>().frame_count();
    world
        .get_resource_or_insert_with(CaptureQueue::default)
        .captures
        .push((
            Capture {
                id,
                reply,
                kind: CaptureKind::Flash,
                total,
                every_n,
                next_due_frame: frame,
                orbit: Vec::new(),
                in_flight: false,
                images: Vec::new(),
                received: 0,
                flash: Some(job),
            },
            Arc::new(Mutex::new(Vec::new())),
        ));
    Outcome::Deferred
}

/// `Last`: drives capture jobs — requests due screenshots, collects results, composes the strip
/// and answers when the job completes.
pub fn advance_captures(world: &mut World) {
    let frame = world.resource::<Time>().frame_count();
    let mut queue = world.remove_resource::<CaptureQueue>().unwrap_or_default();
    if !queue.captures.is_empty() {
        let _span = tracing::debug_span!(target: "regia_agent", "agent_captures").entered();
        tracing::debug!(
            target: "regia_agent",
            jobs = queue.captures.len(),
            frame,
            "advancing capture jobs"
        );
    }
    // A job whose caller already got F0006 (its window closed, or it never presents) frees its
    // slot instead of holding one of the `MAX_CAPTURE_JOBS` for ever (audit-concurrency-1103
    // agent M1).
    queue.captures.retain(|(capture, _)| {
        let gone = crate::server::take_abandoned(&capture.reply);
        if gone {
            tracing::debug!(target: "regia_agent", "capture job dropped: its caller timed out");
        }
        !gone
    });
    let mut finished: Vec<(usize, Result<Value, RpcError>)> = Vec::new();
    for (index, (capture, results)) in queue.captures.iter_mut().enumerate() {
        let delivered = std::mem::take(&mut *results.lock().unwrap_or_else(|e| e.into_inner()));
        match capture.advance(frame, delivered) {
            CaptureAction::Request { orbit } => {
                if let Some(pos) = orbit {
                    move_preview_camera(world, pos);
                }
                if let Some(mut requests) = world.get_resource_mut::<ScreenshotRequests>() {
                    requests.request(
                        None,
                        Box::new({
                            let results = Arc::clone(results);
                            move |result| {
                                results.lock().unwrap_or_else(|e| e.into_inner()).push(result);
                            }
                        }),
                    );
                } else {
                    capture.in_flight = false;
                    finished.push((
                        index,
                        Err(RpcError::new(
                            protocol::UNSUPPORTED,
                            "F0011",
                            "no renderer in this app",
                            "add regia_render::RenderPlugin",
                        )),
                    ));
                }
            }
            CaptureAction::Wait => {}
            CaptureAction::Finished(result) => finished.push((index, result)),
        }
    }
    for (index, result) in finished.into_iter().rev() {
        let (capture, _) = queue.captures.remove(index);
        let response = match result {
            Ok(value) => Response::ok(capture.id, value),
            Err(error) => Response::err(capture.id, error),
        };
        let _ = capture.reply.send(response);
    }
    world.insert_resource(queue);
}

/// The strip answer for a finished job.
fn compose_answer(kind: CaptureKind, images: &[CapturedImage]) -> Result<Value, RpcError> {
    let (label, count) = match kind {
        CaptureKind::Sequence => ("frames", images.len()),
        CaptureKind::Orbit => ("angles", images.len()),
        CaptureKind::Flash => {
            return Err(RpcError::new(
                protocol::UNSUPPORTED,
                "F0015",
                "a flash capture has no strip",
                "the flash job answers from its analyzer",
            ));
        }
    };
    let strip = crate::image::compose_strip(images)
        .and_then(|img| crate::image::png_base64(&img).map(|b64| (img, b64)))
        .map_err(|e| {
            RpcError::new(
                protocol::UNSUPPORTED,
                "F0015",
                format!("cannot compose the strip: {e}"),
                "check logs.read",
            )
        })?;
    let (strip, b64) = strip;
    let mut answer = json!({
        "width": strip.width,
        "height": strip.height,
        "mime": "image/png",
        "png_base64": b64,
    });
    answer[label] = json!(count);
    Ok(answer)
}

/// `seeing.compare_images`: SSIM and a difference image of two PNGs (base64). SSIM per
/// Wang et al., "Image Quality Assessment: From Error Visibility to Structural Similarity",
/// IEEE TIP 2004 — uniform 8×8 windows here (a documented simplification of the paper's
/// 11×11 Gaussian window; see docs/provenance.md).
pub(crate) fn compare_images(params: &Value) -> Result<Value, RpcError> {
    let get_png = |key: &str| -> Result<CapturedImage, RpcError> {
        let text = params
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (base64 PNG) is required")))?;
        let bytes = crate::image::base64_decode(text)
            .map_err(|e| RpcError::invalid_params(format!("`{key}` is not valid base64: {e}")))?;
        crate::image::decode_png_rgba_within(&bytes, crate::image::MAX_INPUT_SIDE)
            .map_err(|e| RpcError::invalid_params(format!("`{key}` is not a decodable PNG: {e}")))
    };
    let a = get_png("a")?;
    let b = get_png("b")?;
    let compared = compare_rgba(&a, &b).map_err(RpcError::invalid_params)?;
    let b64 = crate::image::png_base64(&compared.diff).map_err(RpcError::internal)?;
    Ok(json!({
        "ssim": compared.ssim,
        "max_channel_diff": compared.max_channel_diff,
        "same_size": compared.same_size,
        "width": compared.diff.width,
        "height": compared.diff.height,
        "diff_png_base64": b64,
    }))
}

/// What [`compare_rgba`] measures over the overlap of two images.
pub(crate) struct Compared {
    /// SSIM over 8x8 windows.
    pub ssim: f64,
    /// The largest per-channel difference.
    pub max_channel_diff: u32,
    /// Whether the sizes match.
    pub same_size: bool,
    /// Per-channel absolute difference over the overlap.
    pub diff: CapturedImage,
}

/// Compares two images over their overlap. Errors when they have none.
pub(crate) fn compare_rgba(a: &CapturedImage, b: &CapturedImage) -> Result<Compared, String> {
    let same_size = a.width == b.width && a.height == b.height;
    let width = a.width.min(b.width);
    let height = a.height.min(b.height);
    if width == 0 || height == 0 {
        return Err("the images have no overlapping area".to_string());
    }

    let luma = |img: &CapturedImage, x: usize, y: usize| -> f64 {
        let i = (y * img.width as usize + x) * 4;
        0.299 * f64::from(img.rgba[i])
            + 0.587 * f64::from(img.rgba[i + 1])
            + 0.114 * f64::from(img.rgba[i + 2])
    };

    // SSIM over 8×8 windows (C1/C2 from the paper, dynamic range 255).
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    const WINDOW: usize = 8;
    let mut ssim_sum = 0.0;
    let mut windows = 0usize;
    for wy in (0..height as usize).step_by(WINDOW) {
        for wx in (0..width as usize).step_by(WINDOW) {
            let mut sa = 0.0;
            let mut sb = 0.0;
            let mut saa = 0.0;
            let mut sbb = 0.0;
            let mut sab = 0.0;
            let mut n = 0.0;
            for y in wy..(wy + WINDOW).min(height as usize) {
                for x in wx..(wx + WINDOW).min(width as usize) {
                    let (la, lb) = (luma(a, x, y), luma(b, x, y));
                    sa += la;
                    sb += lb;
                    saa += la * la;
                    sbb += lb * lb;
                    sab += la * lb;
                    n += 1.0;
                }
            }
            let (mua, mub) = (sa / n, sb / n);
            let va = saa / n - mua * mua;
            let vb = sbb / n - mub * mub;
            let cov = sab / n - mua * mub;
            ssim_sum +=
                ((2.0 * mua * mub + C1) * (2.0 * cov + C2)) / ((mua * mua + mub * mub + C1) * (va + vb + C2));
            windows += 1;
        }
    }
    let ssim = ssim_sum / windows.max(1) as f64;

    // Difference image: per-channel absolute difference over the overlap.
    let mut diff = CapturedImage {
        width,
        height,
        rgba: vec![0; (width as usize) * (height as usize) * 4],
    };
    let mut max_channel_diff: u32 = 0;
    for y in 0..height as usize {
        for x in 0..width as usize {
            let ia = (y * a.width as usize + x) * 4;
            let ib = (y * b.width as usize + x) * 4;
            let id = (y * width as usize + x) * 4;
            for c in 0..3 {
                let d = (a.rgba[ia + c] as i32 - b.rgba[ib + c] as i32).unsigned_abs();
                diff.rgba[id + c] = d.min(255) as u8;
                max_channel_diff = max_channel_diff.max(d);
            }
            diff.rgba[id + 3] = 255;
        }
    }
    Ok(Compared {
        ssim,
        max_channel_diff,
        same_size,
        diff,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn asset_preview_path_normalizes_separators() {
        assert_eq!(super::asset_preview_path("assets\\hero.glb"), "assets/hero.glb");
        assert_eq!(super::asset_preview_path("assets/hero.glb"), "assets/hero.glb");
        assert_eq!(super::asset_preview_path("m\\a.glb"), "assets/m/a.glb");
    }

    use super::*;

    /// ADR-7531: this binary's GPU tests and the methods that open a device when the app has none
    /// get one engine device and one viewer device per process, so no test creates or destroys a
    /// device while another test thread names objects (the loader list race, ADR-2113). Two
    /// worlds that each capture also share it.
    #[test]
    fn unit_tests_share_one_engine_and_one_viewer_device() {
        let (Ok(first), Ok(second)) = (headless_render_device(), headless_render_device()) else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        assert_eq!(
            first.device_id(),
            second.device_id(),
            "one engine device per test process"
        );
        assert_eq!(first.device, second.device);
        let mut worlds = [World::new(), World::new()];
        for world in &mut worlds {
            // Only the device step runs: the worlds have no renderer, so the capture refuses after it.
            let _ = offscreen_pixels(world, 4, 4, false, false);
        }
        let opened: Vec<u64> = worlds
            .iter()
            .filter_map(|world| world.get_resource::<regia_render::RenderDevice>())
            .map(regia_render::RenderDevice::device_id)
            .collect();
        assert_eq!(
            opened,
            [first.device_id(); 2],
            "each capture took the shared device"
        );
        let (Ok(viewer_a), Ok(viewer_b)) = (viewer_device(), viewer_device()) else {
            eprintln!("skipping the viewer half: the viewer found no adapter");
            return;
        };
        assert_eq!(viewer_a.1, viewer_b.1, "one viewer device per test process");
    }

    fn image(width: u32, height: u32, value: u8) -> CapturedImage {
        CapturedImage {
            width,
            height,
            rgba: [value, value, value, 255].repeat((width * height) as usize),
        }
    }

    #[test]
    fn identical_images_score_one() {
        let a = image(16, 16, 128);
        let encoded = crate::image::png_base64(&a).expect("encodes");
        let result = compare_images(&json!({ "a": encoded, "b": encoded })).expect("compares");
        assert_eq!(result["ssim"], 1.0);
        assert_eq!(result["same_size"], true);
        assert_eq!(result["max_channel_diff"], 0);
    }

    #[test]
    fn flat_images_differ_structurally() {
        let a = image(16, 16, 10);
        let b = image(16, 16, 240);
        let (ea, eb) = (
            crate::image::png_base64(&a).expect("encodes"),
            crate::image::png_base64(&b).expect("encodes"),
        );
        let result = compare_images(&json!({ "a": ea, "b": eb })).expect("compares");
        // SSIM of two flat images with different brightness is low but not zero (the
        // luminance term keeps it above 0): ~0.08 for 10 vs 240.
        assert!(
            result["ssim"].as_f64().unwrap_or(1.0) < 0.2,
            "flat images: {result}"
        );
        assert_eq!(result["max_channel_diff"], 230);
        let diff = result["diff_png_base64"].as_str().expect("diff png");
        assert!(diff.len() > 8, "difference image present");
    }

    #[test]
    fn noise_structure_lowers_ssim_but_not_to_zero() {
        // Two images with the same mean but different structure.
        let mut a = image(32, 32, 128);
        for (i, px) in a.rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px[0] = if i.is_multiple_of(2) { 200 } else { 56 };
            px[1] = px[0];
            px[2] = px[0];
        }
        let b = image(32, 32, 128);
        let (ea, eb) = (
            crate::image::png_base64(&a).expect("encodes"),
            crate::image::png_base64(&b).expect("encodes"),
        );
        let result = compare_images(&json!({ "a": ea, "b": eb })).expect("compares");
        let ssim = result["ssim"].as_f64().unwrap_or(1.0);
        assert!(ssim < 0.6 && ssim > 0.0, "structure loss shows: {ssim}");
    }

    #[test]
    fn different_sizes_report_the_overlap() {
        let a = image(16, 16, 100);
        let b = image(8, 8, 100);
        let (ea, eb) = (
            crate::image::png_base64(&a).expect("encodes"),
            crate::image::png_base64(&b).expect("encodes"),
        );
        let result = compare_images(&json!({ "a": ea, "b": eb })).expect("compares");
        assert_eq!(result["same_size"], false);
        assert_eq!(result["width"], 8);
        assert_eq!(result["height"], 8);
    }

    #[test]
    fn grayscale_and_rgb_pngs_compare_without_panicking() {
        let encode = |color: png::ColorType, w: u32, h: u32, data: &[u8]| {
            let mut out = Vec::new();
            {
                let mut enc = png::Encoder::new(&mut out, w, h);
                enc.set_color(color);
                enc.set_depth(png::BitDepth::Eight);
                let mut writer = enc.write_header().expect("header");
                writer.write_image_data(data).expect("data");
            }
            crate::image::base64(&out)
        };
        let gray = encode(png::ColorType::Grayscale, 1, 1, &[10]);
        let rgb = encode(png::ColorType::Rgb, 2, 2, &[10; 12]);
        let result = compare_images(&json!({ "a": gray, "b": rgb })).expect("compares");
        assert_eq!(result["width"], 1);
        assert_eq!(result["max_channel_diff"], 0);
    }

    #[test]
    fn capture_and_screenshot_ranges_are_checked_before_use() {
        assert_eq!(capture_every_n(&json!({})).expect("default"), 5);
        assert_eq!(capture_every_n(&json!({ "every_n": 3600 })).expect("max"), 3600);
        for bad in [
            json!(0),
            json!(3601),
            json!(u64::MAX),
            json!(-1),
            json!("x"),
            json!(1.5),
        ] {
            let error = capture_every_n(&json!({ "every_n": bad })).expect_err("refused");
            assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
        }
    }

    #[test]
    fn bad_params_are_clean_errors() {
        assert!(compare_images(&json!({ "a": "!!!" })).is_err());
        assert!(
            compare_images(&json!({ "a": "Zm9v", "b": "Zm9v" })).is_err(),
            "not a PNG"
        );
    }

    #[test]
    fn memory_report_json_lists_every_category() {
        let report = regia_render::MemoryReport::from_allocations(
            "Vulkan",
            [
                ("frame hdr", 64),
                ("terrain heights", 128),
                ("material texture", 32),
            ],
            256,
        );
        let v = memory_report_json(&report, 2);
        assert_eq!(v["allocated_bytes"], 224);
        assert_eq!(v["categories"]["terrain_height"]["bytes"], 128);
        assert_eq!(v["categories"]["shadows"]["count"], 0);
        assert_eq!(v["labels"].as_array().map(Vec::len), Some(2));
        assert_eq!(v["labels"][0]["label"], "terrain heights");
    }

    /// ADR-5860: the whole-device figure is in the answer; a Metal-style report has it without
    /// any allocation list (`available` false), a GL-style one has neither.
    #[test]
    fn memory_report_json_carries_the_device_allocated_bytes() {
        let metal = regia_render::MemoryReport {
            backend: "Metal".to_owned(),
            device_allocated_bytes: Some(96 << 20),
            ..regia_render::MemoryReport::default()
        };
        let v = memory_report_json(&metal, 5);
        assert_eq!(v["available"], false);
        assert_eq!(v["allocated_bytes"], 0);
        assert_eq!(v["device_allocated_bytes"], 96u64 << 20);
        let gl = regia_render::MemoryReport::default();
        assert!(memory_report_json(&gl, 5)["device_allocated_bytes"].is_null());
    }

    #[test]
    fn memory_report_carries_the_peak_rss_and_the_staging_flush() {
        let mut v = json!({ "rss_bytes": 10 });
        let staging = regia_render::StagingFlushStats {
            cap_bytes: 128 << 20,
            flushes: 3,
            flushed_bytes: 3 << 27,
            waits: 2,
            peak_pending_bytes: (128 << 20) + 4096,
        };
        add_load_memory(&mut v, Some(42), &staging);
        assert_eq!(v["rss_bytes"], 10, "the existing fields stay");
        assert_eq!(v["peak_rss_bytes"], 42);
        assert_eq!(v["staging"]["cap_bytes"], 128u64 << 20);
        assert_eq!(v["staging"]["flushes"], 3);
        assert_eq!(v["staging"]["flushed_bytes"], 3u64 << 27);
        assert_eq!(v["staging"]["waits"], 2);
        assert_eq!(v["staging"]["peak_pending_bytes"], (128u64 << 20) + 4096);
        add_load_memory(&mut v, None, &regia_render::StagingFlushStats::default());
        assert!(v["peak_rss_bytes"].is_null(), "no /proc: null, like rss_bytes");
    }

    #[test]
    fn memory_report_without_device_is_unsupported() {
        let mut world = World::new();
        let err = memory_report(&mut world, &json!({})).expect_err("F0011");
        assert_eq!(err.data.as_ref().and_then(|d| d["code"].as_str()), Some("F0011"));
    }

    /// Spec 23 § 3.4 on the runtime `seeing.screenshot` (ADR-7217, formerly ADR-5628's F0013 gap):
    /// view modes and the editor-only overlays (`selection`, `labels`) answer `F0013` before
    /// anything renders; the `camera` and `ui` targets and the colliders / light-radii / bounds /
    /// grid overlays are served, so with no renderer they fail later and differently (no camera,
    /// no UI layer, no view), never `F0013` "not built". Unknown names are parameter errors.
    #[test]
    fn screenshot_serves_camera_ui_and_overlays_and_refuses_only_editor_state() {
        let error_of = |params: Value| {
            let mut world = World::new();
            let (reply, _answers) = crossbeam_channel::unbounded();
            match screenshot(&mut world, &params, json!(1), reply) {
                Outcome::Done(Err(err)) => (
                    err.data
                        .as_ref()
                        .and_then(|d| d["code"].as_str())
                        .map(str::to_string),
                    err.data
                        .as_ref()
                        .and_then(|d| d["reason"].as_str())
                        .map(str::to_string),
                ),
                Outcome::Done(Ok(value)) => panic!("{params}: answered {value}"),
                Outcome::Deferred => panic!("{params}: deferred"),
            }
        };
        for params in [
            json!({ "view_mode": "wireframe" }),
            json!({ "overlays": ["labels"] }),
            json!({ "overlays": ["colliders", "selection"] }),
        ] {
            assert_eq!(error_of(params.clone()).0.as_deref(), Some("F0013"), "{params}");
        }
        assert_eq!(
            error_of(json!({ "overlays": ["selection"] })).1.as_deref(),
            Some("overlay_needs_editor")
        );
        // Served: they fail only because this world has no camera, UI layer or view.
        assert_eq!(
            error_of(json!({ "target": "camera", "name": "Missing" }))
                .0
                .as_deref(),
            Some("F0003")
        );
        assert_eq!(
            error_of(json!({ "target": "ui", "size": [1280, 720] }))
                .1
                .as_deref(),
            Some("screenshot_no_ui_layer")
        );
        // Offscreen with overlays gets as far as the device (a host with no GPU here).
        let mut world = World::new();
        world.insert_resource(GpuDisabled {
            code: "gpu_off".into(),
            reason: "no GPU in this test".into(),
            hint: "none".into(),
        });
        let (reply, _answers) = crossbeam_channel::unbounded();
        let params = json!({ "target": "offscreen", "overlays": ["colliders", "light_radii"] });
        match screenshot(&mut world, &params, json!(1), reply) {
            Outcome::Done(Err(err)) => assert_eq!(
                err.data.as_ref().and_then(|d| d["code"].as_str()),
                Some("gpu_off")
            ),
            _ => panic!("offscreen without a GPU must fail at the device"),
        }
        for params in [
            json!({ "target": "billboard" }),
            json!({ "overlays": ["sparkles"] }),
            json!({ "overlays": [3] }),
            json!({ "target": "camera" }),
            json!({ "target": "ui", "size": [0, 10] }),
        ] {
            assert_eq!(error_of(params.clone()).0.as_deref(), Some("F0002"), "{params}");
        }
    }

    #[test]
    fn pick_entity_is_currently_unsupported() {
        let err = pick_entity(&json!({ "x": 5, "y": 5 })).expect_err("F0013");
        assert_eq!(err.data.as_ref().and_then(|d| d["code"].as_str()), Some("F0013"));
    }

    /// A capture job whose screenshot never comes back (its window closed mid-flight) used to
    /// hold one of the eight job slots for the rest of the session; once its caller has been
    /// answered F0006 the job is dropped (audit-concurrency-1103 agent M1).
    #[test]
    fn a_capture_job_whose_caller_timed_out_frees_its_slot() {
        let mut world = World::new();
        world.insert_resource(Time::default());
        let (reply_tx, _reply_rx) = crossbeam_channel::bounded(1);
        let stuck = Capture {
            id: json!(9),
            reply: reply_tx.clone(),
            kind: CaptureKind::Sequence,
            total: 4,
            every_n: 1,
            next_due_frame: 0,
            orbit: Vec::new(),
            in_flight: true, // waiting for a screenshot that never comes
            images: Vec::new(),
            received: 0,
            flash: None,
        };
        world.insert_resource(CaptureQueue {
            captures: vec![(stuck, Arc::new(Mutex::new(Vec::new())))],
        });
        advance_captures(&mut world);
        assert_eq!(
            world.resource::<CaptureQueue>().captures.len(),
            1,
            "still waiting"
        );
        crate::server::abandon(reply_tx);
        advance_captures(&mut world);
        assert!(
            world.resource::<CaptureQueue>().captures.is_empty(),
            "the job of a caller that timed out is dropped"
        );
    }

    #[test]
    fn capture_state_machine_requests_collects_and_finishes() {
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut capture = Capture {
            id: json!(7),
            reply: reply_tx,
            kind: CaptureKind::Sequence,
            total: 2,
            every_n: 1,
            next_due_frame: 0,
            orbit: Vec::new(),
            in_flight: false,
            images: Vec::new(),
            received: 0,
            flash: None,
        };
        // Frame 0: the first capture is due.
        match capture.advance(0, Vec::new()) {
            CaptureAction::Request { orbit } => assert!(orbit.is_none()),
            other => panic!("expected a request, got {other:?}"),
        }
        assert!(capture.in_flight);
        assert!(matches!(capture.advance(1, Vec::new()), CaptureAction::Wait));

        // The render thread delivers image 1.
        match capture.advance(
            2,
            vec![Ok(CapturedImage {
                width: 2,
                height: 2,
                rgba: vec![7; 16],
            })],
        ) {
            CaptureAction::Request { orbit } => assert!(orbit.is_none()),
            other => panic!("expected the second request, got {other:?}"),
        }
        // Frame gap: not due yet.
        assert!(matches!(capture.advance(2, Vec::new()), CaptureAction::Wait));

        // Image 2 completes the job.
        let action = capture.advance(
            3,
            vec![Ok(CapturedImage {
                width: 2,
                height: 2,
                rgba: vec![9; 16],
            })],
        );
        let CaptureAction::Finished(result) = action else {
            panic!("expected the job to finish");
        };
        let result = result.expect("strip composed");
        assert_eq!(result["frames"], 2);
        assert_eq!(result["width"], 4, "two 2px images side by side");
        // The driver would now reply on capture.reply; assert the channel receives it.
        drop(capture.reply);
        assert!(
            reply_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "channel closed with the capture"
        );
    }

    #[test]
    fn orbit_jobs_report_angles() {
        let (reply_tx, _) = crossbeam_channel::bounded(1);
        // The orbit contact_sheet's own math: N views around the circle, +Z first.
        let angles = 4;
        let orbit: Vec<[f32; 3]> = (0..angles)
            .map(|i| {
                let theta = i as f32 * std::f32::consts::TAU / angles as f32;
                [2.5 * theta.sin(), 0.2, 2.5 * theta.cos()]
            })
            .collect();
        let mut capture = Capture {
            id: json!(1),
            reply: reply_tx,
            kind: CaptureKind::Orbit,
            total: angles,
            every_n: 1,
            next_due_frame: 0,
            orbit,
            in_flight: false,
            images: Vec::new(),
            received: 0,
            flash: None,
        };
        let delivered = || {
            vec![Ok(CapturedImage {
                width: 1,
                height: 1,
                rgba: vec![1; 4],
            })]
        };
        match capture.advance(0, Vec::new()) {
            CaptureAction::Request { orbit: Some(pos) } => {
                assert!((pos[2] - 2.5).abs() < 1e-6, "first orbit view is +Z: {pos:?}");
            }
            other => panic!("expected an orbit request, got {other:?}"),
        }
        match capture.advance(1, delivered()) {
            CaptureAction::Request { orbit: Some(pos) } => {
                assert!((pos[0] - 2.5).abs() < 1e-6, "second orbit view is +X: {pos:?}");
            }
            other => panic!("expected the second orbit request, got {other:?}"),
        }
        match capture.advance(2, delivered()) {
            CaptureAction::Request { orbit: Some(pos) } => {
                assert!((pos[2] + 2.5).abs() < 1e-6, "third orbit view is -Z: {pos:?}");
            }
            other => panic!("expected the third orbit request, got {other:?}"),
        }
        let CaptureAction::Request { .. } = capture.advance(3, delivered()) else {
            panic!("expected the fourth orbit request");
        };
        let CaptureAction::Finished(result) = capture.advance(4, delivered()) else {
            panic!("expected the job to finish");
        };
        assert_eq!(result.expect("answers")["angles"], 4);
    }

    #[test]
    fn a_failed_capture_finishes_with_f0015() {
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut capture = Capture {
            id: json!(2),
            reply: reply_tx,
            kind: CaptureKind::Sequence,
            total: 3,
            every_n: 1,
            next_due_frame: 0,
            orbit: Vec::new(),
            in_flight: false,
            images: Vec::new(),
            received: 0,
            flash: None,
        };
        assert!(matches!(
            capture.advance(0, Vec::new()),
            CaptureAction::Request { .. }
        ));
        let CaptureAction::Finished(Err(error)) = capture.advance(1, vec![Err("window gone".into())]) else {
            panic!("expected a failure");
        };
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("F0015")
        );
        drop(reply_rx);
    }

    #[test]
    fn capture_sequence_params_are_clamped() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.init_resource::<ScreenshotRequests>();
        // No primary window: the handler answers Done(no window) rather than queueing.
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let outcome = capture_sequence(app.world_mut(), &json!({ "frames": 999 }), json!(1), reply_tx);
        match outcome {
            Outcome::Done(Err(error)) => {
                assert_eq!(
                    error.data.as_ref().and_then(|d| d["code"].as_str()),
                    Some("F0003")
                );
            }
            _ => panic!("expected a clean error without a window"),
        }
        drop(reply_rx);
    }

    #[test]
    fn orbit_positions_cover_the_circle() {
        let angles = 4;
        let radius = 2.5_f32;
        let orbit: Vec<[f32; 3]> = (0..angles)
            .map(|i| {
                let theta = i as f32 * std::f32::consts::TAU / angles as f32;
                [radius * theta.sin(), 0.2, radius * theta.cos()]
            })
            .collect();
        assert_eq!(orbit.len(), 4);
        assert!((orbit[0][0] - 0.0).abs() < 1e-6 && (orbit[0][2] - radius).abs() < 1e-6);
        assert!((orbit[1][0] - radius).abs() < 1e-6 && orbit[1][2].abs() < 1e-6);
    }

    /// A 4 Hz full-frame square wave fails the flash check, and the job keeps no pixels.
    #[test]
    fn a_flash_capture_drops_pixels_and_reports_the_burst() {
        let (reply_tx, _) = crossbeam_channel::bounded(1);
        let mut capture = Capture {
            id: json!(3),
            reply: reply_tx,
            kind: CaptureKind::Flash,
            total: 60,
            every_n: 1,
            next_due_frame: 0,
            orbit: Vec::new(),
            in_flight: false,
            images: Vec::new(),
            received: 0,
            flash: Some(crate::seeing_flash::FlashJob::new(
                30.0,
                regia_core::flash::FlashConfig {
                    cell_px: 1,
                    ..regia_core::flash::FlashConfig::default()
                },
            )),
        };
        let mut finished = None;
        for step in 0..=60u32 {
            let delivered = if step == 0 {
                Vec::new()
            } else {
                let time_s = f64::from(step - 1) / 30.0;
                let high = ((time_s * 4.0 * 2.0).floor() * 0.5).fract() == 0.0;
                let byte = if high { 255 } else { 0 };
                vec![Ok(CapturedImage {
                    width: 16,
                    height: 12,
                    rgba: vec![byte; 16 * 12 * 4],
                })]
            };
            match capture.advance(u64::from(step), delivered) {
                CaptureAction::Request { .. } | CaptureAction::Wait => {}
                CaptureAction::Finished(result) => {
                    finished = Some(result);
                    break;
                }
            }
        }
        assert!(capture.images.is_empty(), "a flash job does not keep pixels");
        assert!(capture.flash.is_none(), "the analyzer is finished with the job");
        let report = finished.expect("the burst finishes").expect("report");
        assert_eq!(report["passed"], json!(false));
        assert!(report["max_flashes"].as_u64().expect("count") > 3);
        assert_eq!(report["frames"], json!(60));
    }
}
