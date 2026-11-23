// SPDX-License-Identifier: MPL-2.0
//! The XR simulator in play (spec 58 § 6, ADR-1940): play in editor with no headset.
//!
//! With the toggle on, or `backend: Simulator` in `config/xr.ron`, [`install`] adds `XrPlugin` to
//! the play app, and `XrRenderPlugin` when the play world has a renderer; a game that installed XR
//! itself keeps its own. [`begin`] takes the scripted session to `focused`, so the eyes draw from
//! the first frames of play. The edit world never holds XR: stop drops all of it with the play
//! world.
//!
//! The `XR simulator` window shows both eyes side by side and edits the poses: WASD with Q and E
//! move the head while the pointer is over the eyes, a right drag turns it, and holding Z or X
//! moves the left or the right hand instead. [`XrSimulator`] is the one path into the play world:
//! the window and `editor.xr_simulator` queue edits and [`pump`] applies them once per frame, so an
//! agent and a person move the same poses.
//!
//! The eyes reach the window through a CPU readback every [`PREVIEW_EVERY`] frames, and only while
//! someone is looking: the window is a development view, and the eyes are already the frame's most
//! expensive work.

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::glam::{EulerRot, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_platform::xr::{Eye, Hand, Pose};
use regia_render::{Camera, RenderDevice, RuntimeRenderer};
use regia_scene::prelude::{Name, Transform};
use regia_xr::eye_targets::read_rgba8;
use regia_xr::{
    ScriptState, SimEyeTargets, XrBackendKind, XrConfig, XrPlugin, XrRenderPlugin, XrRenderState, XrRig,
    XrState, box_resize, drive_pose, drive_session_state,
};
use serde_json::{Value, json};

use crate::engine::app::App;
use crate::pie::PieSession;
use crate::viewport::EditorCamera;
use crate::viewport_agent::Handler;

/// The window refreshes the eyes every this many play frames.
pub const PREVIEW_EVERY: u64 = 8;

/// Width of one eye in the window, pixels.
const PREVIEW_WIDTH: u32 = 360;

/// Head and hand speed under the keys, meters per second.
const MOVE_M_PER_S: f32 = 1.5;

/// How far a right drag turns the head, radians per point.
const LOOK_RADIANS_PER_POINT: f32 = 0.005;

/// How far a right drag moves a held hand, meters per point.
const HAND_M_PER_POINT: f32 = 0.002;

/// The head stops short of straight up and down, where yaw has no meaning.
const MAX_PITCH: f32 = 1.5;

/// Head height of the rig placed under the editor camera, meters.
const RIG_HEAD_HEIGHT_M: f32 = 1.7;

/// The head and hands in tracking space, as the window edits them.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct SimPoses {
    /// Head position, meters.
    pub head: Vec3,
    /// Head yaw about +Y, radians.
    pub yaw: f32,
    /// Head pitch, radians.
    pub pitch: f32,
    /// Hand positions (left, right), meters.
    pub hands: [Vec3; 2],
}

impl Default for SimPoses {
    fn default() -> Self {
        SimPoses {
            head: Vec3::new(0.0, RIG_HEAD_HEIGHT_M, 0.0),
            yaw: 0.0,
            pitch: 0.0,
            hands: [Vec3::new(-0.2, 1.2, -0.3), Vec3::new(0.2, 1.2, -0.3)],
        }
    }
}

/// One frame of the window's keys and drag.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SimInput {
    /// W minus S.
    pub forward: f32,
    /// D minus A.
    pub right: f32,
    /// E minus Q.
    pub up: f32,
    /// The right drag this frame, points.
    pub drag: [f32; 2],
    /// The hand whose key is held: the input moves it and leaves the head alone.
    pub hand: Option<Hand>,
    /// Length of the frame, seconds.
    pub seconds: f32,
}

impl SimPoses {
    /// The head's pose.
    #[must_use]
    pub fn head_pose(&self) -> Pose {
        Pose {
            position: self.head,
            orientation: Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, 0.0),
        }
    }

    /// A hand's pose: it faces where the head's yaw faces.
    #[must_use]
    pub fn hand_pose(&self, hand: Hand) -> Pose {
        Pose {
            position: self.hands[hand.index()],
            orientation: Quat::from_rotation_y(self.yaw),
        }
    }

    /// Applies one frame of input. The keys move along the head's yaw, so W stays level with the
    /// head pitched down.
    pub fn step(&mut self, input: &SimInput) {
        let level = Quat::from_rotation_y(self.yaw);
        let walk = Vec3::new(input.right, input.up, -input.forward) * MOVE_M_PER_S * input.seconds;
        let walk = level * walk;
        let [dx, dy] = input.drag;
        match input.hand {
            Some(hand) => {
                let across = level * Vec3::new(dx, -dy, 0.0) * HAND_M_PER_POINT;
                self.hands[hand.index()] += walk + across;
            }
            None => {
                self.head += walk;
                self.yaw -= dx * LOOK_RADIANS_PER_POINT;
                self.pitch = (self.pitch - dy * LOOK_RADIANS_PER_POINT).clamp(-MAX_PITCH, MAX_PITCH);
            }
        }
    }
}

/// A change queued for the play world.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SimEdit {
    /// Every pose (the window's fields).
    Set(SimPoses),
    /// One frame of keys and drag.
    Step(SimInput),
    /// The head (`editor.xr_simulator`): the values given.
    Head {
        /// Position.
        position: Option<Vec3>,
        /// Yaw, radians.
        yaw: Option<f32>,
        /// Pitch, radians.
        pitch: Option<f32>,
    },
    /// One hand's position.
    Hand(Hand, Vec3),
}

impl SimEdit {
    fn apply(self, poses: &mut SimPoses) {
        match self {
            SimEdit::Set(all) => *poses = all,
            SimEdit::Step(input) => poses.step(&input),
            SimEdit::Head { position, yaw, pitch } => {
                poses.head = position.unwrap_or(poses.head);
                poses.yaw = yaw.unwrap_or(poses.yaw);
                poses.pitch = pitch.unwrap_or(poses.pitch).clamp(-MAX_PITCH, MAX_PITCH);
            }
            SimEdit::Hand(hand, position) => poses.hands[hand.index()] = position,
        }
    }
}

/// What the running session reports to the edit world.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SimSnapshot {
    /// The play world runs the simulator.
    pub active: bool,
    /// The poses.
    pub poses: SimPoses,
    /// The XR app state's name.
    pub app_state: String,
    /// Stereo frames the eyes drew.
    pub rendered_frames: u64,
}

/// One readback of both eyes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EyePreview {
    /// The play frame it was read on.
    pub frame: u64,
    /// Size of each eye, pixels.
    pub size: [u32; 2],
    /// Tightly packed RGBA8, left then right.
    pub eyes: [Vec<u8>; 2],
}

/// The simulator's state in the edit world.
#[derive(Resource, Default)]
pub struct XrSimulator {
    /// Play runs the simulator whatever `config/xr.ron` names.
    pub toggle: bool,
    /// Edits waiting for the next [`pump`].
    pub edits: Vec<SimEdit>,
    /// The session, as of the last [`pump`].
    pub snapshot: SimSnapshot,
    /// The last readback, while someone is looking.
    pub preview: Option<EyePreview>,
    /// The next [`pump`] due for a refresh reads the eyes.
    pub preview_wanted: bool,
    textures: Option<(u64, [egui::TextureHandle; 2])>,
}

/// Adds XR to the play app when the toggle or the project asks for the simulator. True when it
/// did. Runs after the game's plugin: a game that installed XR keeps it. A broken `config/xr.ron`
/// never stops play.
pub fn install(edit: &World, app: &mut App) -> bool {
    if app.world().contains_resource::<XrState>() {
        return false;
    }
    let toggle = edit.get_resource::<XrSimulator>().is_some_and(|sim| sim.toggle);
    let file = crate::mesh_source::project_root(edit).map(|root| crate::xr_settings::read_config(&root));
    let config = match file {
        Some(Ok(config)) => config,
        Some(Err(err)) => {
            tracing::warn!(target: "regia_editor", "XR simulator: {err}");
            XrConfig::default()
        }
        None => XrConfig::default(),
    };
    if !toggle && config.backend != XrBackendKind::Simulator {
        return false;
    }
    app.init_resource::<regia_input::InputInjection>();
    app.add_plugins(XrPlugin::new(XrConfig {
        backend: XrBackendKind::Simulator,
        required: false,
        ..config
    }));
    if app.world().contains_resource::<RuntimeRenderer>() {
        app.add_plugins(XrRenderPlugin);
    }
    app.init_resource::<SimPoses>();
    place_rig(edit, app.world_mut());
    true
}

/// Gives the play world a rig under the editor camera, when the level has none, and a camera, which
/// a frame needs for its settings (the eyes replace its matrices).
fn place_rig(edit: &World, play: &mut World) {
    let camera = edit.get_resource::<EditorCamera>();
    let eye = camera.map_or(Vec3::new(0.0, RIG_HEAD_HEIGHT_M, 0.0), EditorCamera::eye);
    let forward = camera.map_or(Vec3::NEG_Z, EditorCamera::forward);
    let origin = eye - Vec3::Y * RIG_HEAD_HEIGHT_M;
    let transform = Transform {
        translation: origin,
        rotation: Quat::from_rotation_y((-forward.x).atan2(-forward.z)),
        ..Transform::default()
    };
    if play
        .query_filtered::<(), With<XrRig>>()
        .iter(play)
        .next()
        .is_none()
    {
        play.spawn((Name::new("XR rig"), XrRig, transform));
    }
    if play.query::<&Camera>().iter(play).next().is_none() {
        play.spawn((Name::new("XR view"), Camera::default(), transform));
    }
}

/// Takes the scripted session to `focused`. Runs after play's first frame.
pub fn begin(app: &mut App) {
    let world = app.world_mut();
    let Some(poses) = world.get_resource::<SimPoses>().copied() else {
        return;
    };
    if let Err(err) = drive_session_state(world, ScriptState::Focused) {
        tracing::warn!(target: "regia_editor", "XR simulator: {err}");
        return;
    }
    drive(world, &poses);
}

fn drive(play: &mut World, poses: &SimPoses) {
    let hands = Hand::BOTH.map(|hand| Some(poses.hand_pose(hand)));
    if let Err(err) = drive_pose(play, Some(poses.head_pose()), hands) {
        tracing::warn!(target: "regia_editor", "XR simulator: {err}");
    }
}

/// Reads both eyes, what a headset shows when the frame composed that, scaled to the window.
fn read_preview(play: &World, frame: u64) -> Option<EyePreview> {
    let targets = play.get_resource::<SimEyeTargets>()?;
    let device = play.get_resource::<RenderDevice>()?;
    let render = play.get_resource::<XrRenderState>();
    let extent = regia_xr::EyeTargets::extent(targets);
    let width = PREVIEW_WIDTH.min(extent.x);
    let height = (u64::from(extent.y) * u64::from(width) / u64::from(extent.x)).max(1) as u32;
    let read = |eye: Eye| -> Option<Vec<u8>> {
        let texture = render
            .and_then(|r| r.display_texture(eye))
            .unwrap_or_else(|| targets.texture(eye));
        let rgba = read_rgba8(device, texture, extent).ok()?;
        let mut small = box_resize(&rgba, extent.x, extent.y, width, height)?;
        for alpha in small.iter_mut().skip(3).step_by(4) {
            *alpha = u8::MAX;
        }
        Some(small)
    };
    Some(EyePreview {
        frame,
        size: [width, height],
        eyes: [read(Eye::Left)?, read(Eye::Right)?],
    })
}

/// Once per editor frame: applies the queued edits to the session and reports it back.
#[tracing::instrument(target = "regia_editor", skip_all, name = "xr_simulator_pump")]
pub fn pump(edit: &mut World, session: Option<&mut PieSession>) {
    let mut sim = edit.get_resource_or_init::<XrSimulator>();
    let running = session.and_then(|session| {
        let frame = session.frames;
        let play = session.app.world_mut();
        let poses = play.get_resource::<SimPoses>().copied()?;
        Some((play, poses, frame))
    });
    let Some((play, mut poses, frame)) = running else {
        sim.edits.clear();
        sim.snapshot = SimSnapshot::default();
        sim.preview = None;
        sim.preview_wanted = false;
        return;
    };
    let before = poses;
    for change in sim.edits.drain(..) {
        change.apply(&mut poses);
    }
    if poses != before {
        play.insert_resource(poses);
        drive(play, &poses);
    }
    if sim.preview_wanted && frame.is_multiple_of(PREVIEW_EVERY) {
        let preview = read_preview(play, frame);
        if preview.is_some() {
            sim.preview = preview;
            sim.preview_wanted = false;
        }
    }
    sim.snapshot = SimSnapshot {
        active: true,
        poses,
        app_state: play
            .get_resource::<XrState>()
            .map_or_else(String::new, |state| state.app.name().to_string()),
        rendered_frames: play
            .get_resource::<XrRenderState>()
            .map_or(0, |render| render.stats().frames),
    };
}

/// Reads the keys and the drag over the eyes.
fn read_input(ui: &egui::Ui, eyes: &egui::Response) -> SimInput {
    let keys = eyes.hovered() && !ui.ctx().egui_wants_keyboard_input();
    let delta = eyes.drag_delta();
    let drag = if eyes.dragged_by(egui::PointerButton::Secondary) {
        [delta.x, delta.y]
    } else {
        [0.0, 0.0]
    };
    ui.input(|input| {
        let axis = |plus, minus| {
            if !keys {
                return 0.0;
            }
            f32::from(u8::from(input.key_down(plus))) - f32::from(u8::from(input.key_down(minus)))
        };
        SimInput {
            forward: axis(egui::Key::W, egui::Key::S),
            right: axis(egui::Key::D, egui::Key::A),
            up: axis(egui::Key::E, egui::Key::Q),
            drag,
            hand: match (input.key_down(egui::Key::Z), input.key_down(egui::Key::X)) {
                (true, _) if keys => Some(Hand::Left),
                (_, true) if keys => Some(Hand::Right),
                _ => None,
            },
            seconds: input.stable_dt.min(0.1),
        }
    })
}

fn vec3_fields(ui: &mut egui::Ui, label: &str, value: &mut Vec3) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(&mut value.x).speed(0.01).suffix(" m"));
        ui.add(egui::DragValue::new(&mut value.y).speed(0.01).suffix(" m"));
        ui.add(egui::DragValue::new(&mut value.z).speed(0.01).suffix(" m"));
    });
}

/// The window's contents.
fn simulator_contents(ui: &mut egui::Ui, sim: &mut XrSimulator) {
    ui.weak(format!(
        "{} · {} stereo frames",
        sim.snapshot.app_state, sim.snapshot.rendered_frames
    ));
    if let Some(preview) = sim.preview.as_ref().filter(|preview| {
        sim.textures
            .as_ref()
            .is_none_or(|(frame, _)| *frame != preview.frame)
    }) {
        let [width, height] = preview.size.map(|side| side as usize);
        let load = |name: &str, rgba: &[u8]| {
            ui.ctx().load_texture(
                name,
                egui::ColorImage::from_rgba_unmultiplied([width, height], rgba),
                egui::TextureOptions::LINEAR,
            )
        };
        let [left, right] = &preview.eyes;
        sim.textures = Some((
            preview.frame,
            [load("xr-eye-left", left), load("xr-eye-right", right)],
        ));
    }
    let size = sim.preview.as_ref().map_or([PREVIEW_WIDTH, 200], |p| p.size);
    let eye = egui::vec2(size[0] as f32, size[1] as f32);
    let eyes = ui.allocate_response(egui::vec2(eye.x * 2.0, eye.y), egui::Sense::drag());
    let area = eyes.rect;
    let painter = ui.painter_at(area);
    painter.rect_filled(area, 0.0, egui::Color32::BLACK);
    if let Some((_, textures)) = &sim.textures {
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        for (index, texture) in textures.iter().enumerate() {
            let min = area.min + egui::vec2(eye.x * index as f32, 0.0);
            let rect = egui::Rect::from_min_size(min, eye);
            painter.image(texture.id(), rect, uv, egui::Color32::WHITE);
        }
    }
    let input = read_input(ui, &eyes);
    let idle = SimInput {
        seconds: input.seconds,
        ..SimInput::default()
    };
    if input != idle {
        sim.edits.push(SimEdit::Step(input));
    }
    ui.weak("Over the eyes: WASD, Q and E move, right drag turns. Hold Z or X for the left or right hand.");
    let mut poses = sim.snapshot.poses;
    vec3_fields(ui, "Head", &mut poses.head);
    ui.horizontal(|ui| {
        ui.label("Yaw, pitch");
        ui.add(egui::DragValue::new(&mut poses.yaw).speed(0.01).suffix(" rad"));
        ui.add(
            egui::DragValue::new(&mut poses.pitch)
                .speed(0.01)
                .range(-MAX_PITCH..=MAX_PITCH)
                .suffix(" rad"),
        );
    });
    let [left, right] = &mut poses.hands;
    vec3_fields(ui, "Left hand", left);
    vec3_fields(ui, "Right hand", right);
    if poses != sim.snapshot.poses {
        sim.edits.push(SimEdit::Set(poses));
    }
}

/// The `XR simulator` window, while play runs the simulator.
#[tracing::instrument(target = "regia_editor", skip_all, name = "xr_simulator_panel")]
pub fn xr_simulator_panel(ctx: &egui::Context, world: &mut World) {
    let Some(mut sim) = world.get_resource_mut::<XrSimulator>() else {
        return;
    };
    if !sim.snapshot.active {
        sim.textures = None;
        return;
    }
    let shown = egui::Window::new("XR simulator")
        .default_open(true)
        .show(ctx, |ui| simulator_contents(ui, &mut sim));
    sim.preview_wanted |= shown.is_some_and(|window| window.inner.is_some());
}

fn invalid(key: &str, reason: &str) -> RpcError {
    let mut e = RpcError::new(
        INVALID_PARAMS,
        "F0002",
        format!("{key}: {reason}"),
        "editor.xr_simulator without parameters reports the session",
    );
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!("xr_invalid_value"));
    }
    e
}

fn parse_vec3(key: &str, value: &Value) -> Result<Vec3, RpcError> {
    let numbers: Option<Vec<f32>> = value
        .as_array()
        .and_then(|items| items.iter().map(|n| n.as_f64().map(|n| n as f32)).collect());
    match numbers.as_deref() {
        Some(&[x, y, z]) if x.is_finite() && y.is_finite() && z.is_finite() => Ok(Vec3::new(x, y, z)),
        _ => Err(invalid(key, "[x, y, z], finite meters")),
    }
}

fn parse_angle(key: &str, value: Option<&Value>) -> Result<Option<f32>, RpcError> {
    match value {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|n| n as f32)
            .filter(|n| n.is_finite())
            .map(Some)
            .ok_or_else(|| invalid(key, "finite radians")),
    }
}

fn parse_edits(params: &Value) -> Result<Vec<SimEdit>, RpcError> {
    let mut edits = Vec::new();
    if let Some(head) = params.get("head") {
        let position = head
            .get("position")
            .map(|p| parse_vec3("head.position", p))
            .transpose()?;
        edits.push(SimEdit::Head {
            position,
            yaw: parse_angle("head.yaw", head.get("yaw"))?,
            pitch: parse_angle("head.pitch", head.get("pitch"))?,
        });
    }
    for (key, hand) in [("left", Hand::Left), ("right", Hand::Right)] {
        if let Some(position) = params.get(key) {
            edits.push(SimEdit::Hand(hand, parse_vec3(key, position)?));
        }
    }
    Ok(edits)
}

fn image_of(size: [u32; 2], rgba: &[u8]) -> Result<String, RpcError> {
    let [width, height] = size.map(|side| side as usize);
    let image = egui::ColorImage::from_rgba_unmultiplied([width, height], rgba);
    crate::render::viewport_png_base64(&image).map_err(RpcError::internal)
}

/// `editor.xr_simulator { enabled?, head?, left?, right?, capture? }`.
fn editor_xr_simulator(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = match params.get("enabled") {
        None => None,
        Some(v) => Some(v.as_bool().ok_or_else(|| invalid("enabled", "true or false"))?),
    };
    let edits = parse_edits(params)?;
    let capture = match params.get("capture") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| invalid("capture", "true or false"))?,
    };
    let mut sim = world.get_resource_or_init::<XrSimulator>();
    sim.toggle = enabled.unwrap_or(sim.toggle);
    let queued = edits.len();
    if sim.snapshot.active {
        sim.edits.extend(edits);
    }
    sim.preview_wanted |= capture;
    let snapshot = &sim.snapshot;
    let poses = snapshot.active.then(|| {
        json!({
            "head": snapshot.poses.head.to_array(),
            "yaw": snapshot.poses.yaw,
            "pitch": snapshot.poses.pitch,
            "left": snapshot.poses.hands[0].to_array(),
            "right": snapshot.poses.hands[1].to_array(),
        })
    });
    let preview = match sim.preview.as_ref().filter(|_| capture) {
        None => Value::Null,
        Some(preview) => json!({
            "frame": preview.frame,
            "width": preview.size[0],
            "height": preview.size[1],
            "format": "png",
            "eyes": [image_of(preview.size, &preview.eyes[0])?, image_of(preview.size, &preview.eyes[1])?],
        }),
    };
    Ok(json!({
        "enabled": sim.toggle,
        "active": snapshot.active,
        "app_state": snapshot.active.then(|| snapshot.app_state.clone()),
        "rendered_frames": snapshot.rendered_frames,
        "poses": poses,
        "queued": if snapshot.active { queued } else { 0 },
        "preview": preview,
    }))
}

/// The simulator's methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.xr_simulator",
            tool: "xr_simulator",
            description: "Play in editor with the scripted XR runtime: `enabled` sets the toggle for the next play; while it runs `head`, `left` and `right` move the poses at the next frame, and `capture` asks for both eyes (poll: `preview` stays null until a frame has read them). Errors are F0002 with data.reason xr_invalid_value.",
            params: &[
                ParamSchema {
                    name: "enabled",
                    kind: "boolean",
                    required: false,
                    description: "Run the simulator in play whatever config/xr.ron names.",
                },
                ParamSchema {
                    name: "head",
                    kind: "object",
                    required: false,
                    description: "{ position?: [x, y, z], yaw?, pitch? }: tracking space, meters and radians.",
                },
                ParamSchema {
                    name: "left",
                    kind: "array",
                    required: false,
                    description: "Left hand position, [x, y, z].",
                },
                ParamSchema {
                    name: "right",
                    kind: "array",
                    required: false,
                    description: "Right hand position, [x, y, z].",
                },
                ParamSchema {
                    name: "capture",
                    kind: "boolean",
                    required: false,
                    description: "Read both eyes; the answer carries the last readback.",
                },
            ],
            result: "{ enabled, active, app_state: string | null, rendered_frames, poses: { head, yaw, pitch, left, right } | null, queued, preview: { frame, width, height, format, eyes: [left, right] } | null }",
            mutating: true,
            permission: Permission::Play,
        },
        editor_xr_simulator as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pie::{PieRequest, start_play, stop_play};
    use regia_core::id::Guid;
    use regia_render::{DirectionalLightUniform, LabMaterial, LabMesh, LabScene, LabVertex};
    use regia_scene::level::{Level, LevelLibrary, LoadMode, open_level};
    use regia_scene::prelude::LevelId;
    use regia_xr::{XR_CONFIG_PATH, XrRenderProfile};

    fn second(forward: f32) -> SimInput {
        SimInput {
            forward,
            seconds: 1.0,
            ..SimInput::default()
        }
    }

    #[test]
    fn simulator_params_use_the_boolean_kind() {
        let (schema, _) = methods().into_iter().next().expect("method");
        let kinds: Vec<_> = schema
            .params
            .iter()
            .map(|param| (param.name, param.kind))
            .collect();
        assert!(kinds.contains(&("enabled", "boolean")), "{kinds:?}");
        assert!(kinds.contains(&("capture", "boolean")), "{kinds:?}");
    }

    #[test]
    fn the_keys_walk_along_the_heads_yaw_and_stay_level() {
        let mut poses = SimPoses {
            yaw: std::f32::consts::FRAC_PI_2,
            pitch: -1.0,
            ..SimPoses::default()
        };
        let start = poses.head;
        poses.step(&second(1.0));
        let moved = poses.head - start;
        assert!(moved.y.abs() < 1e-5, "{moved:?}");
        assert!((moved.x + MOVE_M_PER_S).abs() < 1e-5, "{moved:?}");
        assert!(moved.z.abs() < 1e-5, "{moved:?}");
    }

    #[test]
    fn a_right_drag_turns_the_head_and_stops_short_of_the_pole() {
        let mut poses = SimPoses::default();
        poses.step(&SimInput {
            drag: [100.0, -100_000.0],
            ..SimInput::default()
        });
        assert!((poses.yaw + 100.0 * LOOK_RADIANS_PER_POINT).abs() < 1e-5);
        assert_eq!(poses.pitch, MAX_PITCH);
        assert_eq!(poses.head, SimPoses::default().head, "a drag does not walk");
    }

    #[test]
    fn a_held_hand_takes_the_input_and_the_head_stays() {
        let mut poses = SimPoses::default();
        poses.step(&SimInput {
            hand: Some(Hand::Right),
            drag: [100.0, 0.0],
            ..second(1.0)
        });
        let rest = SimPoses::default();
        assert_eq!((poses.head, poses.yaw), (rest.head, rest.yaw));
        assert_eq!(poses.hands[0], rest.hands[0]);
        let want = rest.hands[1] + Vec3::new(100.0 * HAND_M_PER_POINT, 0.0, -MOVE_M_PER_S);
        assert!((poses.hands[1] - want).length() < 1e-5, "{:?}", poses.hands[1]);
    }

    #[test]
    fn the_same_input_gives_the_same_poses() {
        let run = || {
            let mut poses = SimPoses::default();
            for step in 0..240 {
                let t = step as f32;
                poses.step(&SimInput {
                    forward: (t * 0.1).sin(),
                    right: (t * 0.07).cos(),
                    drag: [(t * 0.3).sin() * 4.0, (t * 0.2).cos() * 2.0],
                    seconds: 1.0 / 60.0,
                    ..SimInput::default()
                });
            }
            poses
        };
        assert_eq!(run(), run());
    }

    fn editor_app(root: &std::path::Path, text: Option<&str>) -> App {
        if let Some(text) = text {
            std::fs::create_dir_all(root.join("config")).expect("config dir");
            std::fs::write(root.join(XR_CONFIG_PATH), text).expect("write");
        }
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let id = LevelId(Guid::new_random());
        let level = Level::from_ron_str(id, "( format_version: 1, entities: [] )").expect("parse");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        app
    }

    const SIMULATOR: &str = "(format_version: 1, backend: Simulator)";

    #[test]
    fn play_stays_free_of_xr_unless_something_asks_for_the_simulator() {
        let root = regia_test_utils::unique_temp_dir("xr_sim_off");
        let mut app = editor_app(&root, None);
        let session = start_play(app.world_mut(), None).expect("play");
        assert!(!session.app.world().contains_resource::<XrState>());
        assert!(!session.app.world().contains_resource::<SimPoses>());
        stop_play(session);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_broken_file_does_not_stop_play() {
        let root = regia_test_utils::unique_temp_dir("xr_sim_broken");
        let mut app = editor_app(&root, Some("(format_version: 1, surprise: true)"));
        let session = start_play(app.world_mut(), None).expect("play");
        assert!(!session.app.world().contains_resource::<XrState>());
        stop_play(session);
        app.insert_resource(XrSimulator {
            toggle: true,
            ..XrSimulator::default()
        });
        let session = start_play(app.world_mut(), None).expect("play with the toggle");
        assert!(session.app.world().contains_resource::<XrState>());
        stop_play(session);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_project_file_runs_the_session_and_the_status_reaches_the_toolbar() {
        let root = regia_test_utils::unique_temp_dir("xr_sim_play");
        let mut app = editor_app(&root, Some(SIMULATOR));
        let mut slot = None;
        let toast = crate::pie::apply_request(app.world_mut(), &mut slot, PieRequest::Play, None);
        assert!(slot.is_some(), "{toast}");
        crate::pie::sync_status(app.world_mut(), slot.as_ref());
        let status = app.world().resource::<crate::pie::PieStatus>().clone();
        assert_eq!(status.xr.as_deref(), Some("XR simulator · Focused · no runtime"));

        pump(app.world_mut(), slot.as_mut());
        let sim = app.world().resource::<XrSimulator>();
        assert!(sim.snapshot.active);
        assert_eq!(sim.snapshot.app_state, "Focused");
        assert_eq!(sim.snapshot.poses, SimPoses::default());

        crate::pie::apply_request(app.world_mut(), &mut slot, PieRequest::Stop, None);
        crate::pie::sync_status(app.world_mut(), slot.as_ref());
        assert_eq!(app.world().resource::<crate::pie::PieStatus>().xr, None);
        pump(app.world_mut(), slot.as_mut());
        assert!(!app.world().resource::<XrSimulator>().snapshot.active);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_agent_moves_the_head_and_a_hand_through_the_same_queue_as_the_window() {
        let root = regia_test_utils::unique_temp_dir("xr_sim_agent");
        let mut app = editor_app(&root, None);
        let answer = editor_xr_simulator(app.world_mut(), &json!({ "enabled": true })).expect("enable");
        assert_eq!(answer["enabled"], json!(true));
        assert_eq!(answer["active"], json!(false));
        assert_eq!(answer["poses"], Value::Null);

        let mut session = Some(start_play(app.world_mut(), None).expect("play"));
        pump(app.world_mut(), session.as_mut());
        let answer = editor_xr_simulator(
            app.world_mut(),
            &json!({ "head": { "position": [1.0, 1.5, 2.0], "yaw": 0.5 }, "right": [0.3, 1.0, -0.2] }),
        )
        .expect("move");
        assert_eq!(answer["queued"], json!(2));
        pump(app.world_mut(), session.as_mut());
        // A controller reads as moved after the next frame's action sync, as on a real runtime.
        let live = session.as_mut().expect("session");
        assert_eq!(live.frame(), None);
        let rt = live.app.world().resource::<regia_platform::xr::XrRuntime>();
        let sample = rt.backend().head_sample();
        assert!((sample.pose.position - Vec3::new(1.0, 1.5, 2.0)).length() < 1e-5);
        let right = rt.backend().controller(Hand::Right);
        assert!(right.active);
        assert!((right.grip.pose.position - Vec3::new(0.3, 1.0, -0.2)).length() < 1e-5);

        let answer = editor_xr_simulator(app.world_mut(), &json!({})).expect("read");
        assert_eq!(answer["app_state"], json!("Focused"));
        assert_eq!(answer["poses"]["head"], json!([1.0, 1.5, 2.0]));
        assert_eq!(answer["queued"], json!(0));

        for bad in [
            json!({ "enabled": "yes" }),
            json!({ "left": [1.0, 2.0] }),
            json!({ "head": { "position": "here" } }),
            json!({ "head": { "yaw": "left" } }),
            json!({ "capture": 1 }),
        ] {
            let e = editor_xr_simulator(app.world_mut(), &bad).expect_err("bad parameter");
            let reason = e.data.and_then(|data| data.get("reason").cloned());
            assert_eq!(reason, Some(json!("xr_invalid_value")), "{bad}");
        }
        stop_play(session.take().expect("session"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rig_and_a_view_are_placed_under_the_editor_camera() {
        let root = regia_test_utils::unique_temp_dir("xr_sim_rig");
        let mut app = editor_app(&root, Some(SIMULATOR));
        app.world_mut().insert_resource(EditorCamera {
            target: Vec3::new(4.0, 2.7, -3.0),
            distance: 0.5,
            ..EditorCamera::default()
        });
        let eye = app.world().resource::<EditorCamera>().eye();
        let mut session = start_play(app.world_mut(), None).expect("play");
        let world = session.app.world_mut();
        let rigs: Vec<Transform> = world
            .query_filtered::<&Transform, With<XrRig>>()
            .iter(world)
            .copied()
            .collect();
        assert_eq!(rigs.len(), 1);
        assert!((rigs[0].translation + Vec3::Y * RIG_HEAD_HEIGHT_M - eye).length() < 1e-5);
        assert_eq!(world.query::<&Camera>().iter(world).count(), 1);
        stop_play(session);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn draw(world: &mut World, events: Vec<egui::Event>) {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| xr_simulator_panel(ui.ctx(), world));
        output.textures_delta.clear();
    }

    #[test]
    fn the_window_draws_headless_and_asks_for_the_eyes_only_while_a_session_runs() {
        let root = regia_test_utils::unique_temp_dir("xr_sim_window");
        let mut app = editor_app(&root, Some(SIMULATOR));
        pump(app.world_mut(), None);
        draw(app.world_mut(), Vec::new());
        assert!(!app.world().resource::<XrSimulator>().preview_wanted);

        let mut session = Some(start_play(app.world_mut(), None).expect("play"));
        pump(app.world_mut(), session.as_mut());
        draw(app.world_mut(), Vec::new());
        assert!(app.world().resource::<XrSimulator>().preview_wanted);
        stop_play(session.take().expect("session"));
        pump(app.world_mut(), None);
        assert!(!app.world().resource::<XrSimulator>().preview_wanted);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A lit wall the rig at the origin looks at.
    fn wall() -> LabScene {
        let (near, half) = (-30.0, 60.0);
        LabScene {
            width: 64,
            height: 48,
            eye: [0.0, 0.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 1.0,
            near: 0.05,
            far: 100.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 20_000.0,
            double_sided: true,
            post: None,
            directional: DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], 20_000.0),
            local_lights: Vec::new(),
            materials: vec![LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                ..LabMaterial::default()
            }],
            meshes: vec![LabMesh {
                vertices: vec![
                    LabVertex::from_pos_normal_uv([-half, -half, near], [0.0, 0.0, 1.0], [0.0, 1.0]),
                    LabVertex::from_pos_normal_uv([half, -half, near], [0.0, 0.0, 1.0], [1.0, 1.0]),
                    LabVertex::from_pos_normal_uv([half, half, near], [0.0, 0.0, 1.0], [1.0, 0.0]),
                    LabVertex::from_pos_normal_uv([-half, half, near], [0.0, 0.0, 1.0], [0.0, 0.0]),
                ],
                indices: vec![0, 1, 2, 0, 2, 3],
                ..LabMesh::unskinned()
            }],
        }
    }

    /// The whole path on a software adapter: a level with a mesh, play with the toggle, the eyes
    /// through the play world's renderer, and the readback the window shows.
    #[test]
    fn play_draws_both_eyes_and_the_window_reads_them() {
        let Ok(device) = crate::render::headless_device() else {
            eprintln!("skipped: no headless GPU");
            return;
        };
        let root = regia_test_utils::unique_temp_dir("xr_sim_eyes");
        let mut app = editor_app(&root, Some(SIMULATOR));
        app.insert_resource(device);
        app.init_resource::<RuntimeRenderer>();
        let mut session = Some(start_play(app.world_mut(), None).expect("play"));
        {
            let world = session.as_mut().expect("session").app.world_mut();
            world.resource_mut::<XrRenderProfile>().recommended_extent = regia_core::glam::UVec2::new(64, 48);
            world.resource_mut::<XrRenderProfile>().per_eye_history = false;
            regia_render::spawn_lab_scene(world, &wall());
        }
        app.world_mut()
            .get_resource_or_init::<XrSimulator>()
            .preview_wanted = true;
        for _ in 0..(PREVIEW_EVERY * 2) {
            let live = session.as_mut().expect("session");
            crate::render::extract(live.app.world_mut());
            assert_eq!(live.frame(), None);
            pump(app.world_mut(), session.as_mut());
        }
        let sim = app.world().resource::<XrSimulator>();
        assert!(sim.snapshot.rendered_frames > 0, "{:?}", sim.snapshot);
        let preview = sim.preview.as_ref().expect("the eyes were read");
        assert_eq!(preview.size, [64, 48]);
        assert!(
            preview.eyes[0]
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[..3] != [0, 0, 0])
        );
        assert_eq!(preview.eyes[0].len(), 64 * 48 * 4);
        let capture = editor_xr_simulator(app.world_mut(), &json!({ "capture": true })).expect("capture");
        assert_eq!(capture["preview"]["format"], json!("png"));
        stop_play(session.take().expect("session"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
