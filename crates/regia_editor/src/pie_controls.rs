// SPDX-License-Identifier: MPL-2.0
//! Play start options, time scale and mute (spec addendum § 15.1, ADR-7829).
//!
//! - **Start.** A session starts from the level's player start (the default: the game's own spawn
//!   rules decide), from the **editor camera** or from **one chosen player start**. The play world is
//!   a copy of the edit world, so the choice is applied to the copy before the game's `on_load`:
//!   every other `PlayerStart` loses its marker in the copy (a game's spawn rules can only pick the
//!   chosen one), and for the camera a play-only `PlayerStart` is spawned at the editor camera's
//!   pose. A play world that has no player after its first frame (a game with no player, or
//!   Simulate) gets its window cameras placed at the editor camera's pose instead. The edit world is
//!   never touched.
//! - **Time scale.** The play world's gameplay [`Time::time_scale`], 0.1..=4 (pause stays the Pause
//!   button). It is per session: a new session starts at 1 unless the request sets it.
//! - **Mute.** Mutes the play world's audio (the root bus of the play world's `regia_audio::Audio`,
//!   when the game has one), never the editor's own sounds (the asset preview has its own output).
//!   A game's own root mute is put back on unmute. Per session, like the time scale.
//!
//! The toolbar's start choice is [`PlayStartChoice`] in the edit world (view state: never on the
//! undo stack, never saved with the level); `editor.play { start }` overrides it for one session.

use crate::engine::app::Time;
use crate::engine::ecs::prelude::*;
use regia_core::id::Guid;
use regia_scene::components::EntityGuid;
use regia_scene::prelude::{GlobalTransform, Name, Transform};

use crate::viewport::EditorCamera;

/// Where a play session's player starts (`editor.play { start }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayStart {
    /// The level's player start, chosen by the game's spawn rules (the default).
    #[default]
    PlayerStart,
    /// The editor camera's pose: the player spawns there, or, with no player, the play camera is
    /// placed there.
    Camera,
    /// One player start of the level, by its entity guid.
    Entity(Guid),
}

impl PlayStart {
    /// Stable name for the protocol (`player_start`, `camera`, `entity`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlayerStart => "player_start",
            Self::Camera => "camera",
            Self::Entity(_) => "entity",
        }
    }
}

/// The Play dropdown's start choice (edit-world view state). Play, Simulate and Alt+P use it unless
/// a request names its own start.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlayStartChoice(pub PlayStart);

/// Smallest play time scale (`editor.play_controls { time_scale }`); 0 is the Pause button.
pub const MIN_TIME_SCALE: f32 = 0.1;
/// Largest play time scale.
pub const MAX_TIME_SCALE: f32 = 4.0;

/// A play time scale in hundredths (`100` = real time), kept whole so requests stay `Eq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimeScale(u16);

impl TimeScale {
    /// Real time.
    pub const ONE: Self = Self(100);

    /// `scale` rounded to hundredths, when it is within [`MIN_TIME_SCALE`]..=[`MAX_TIME_SCALE`].
    #[must_use]
    pub fn new(scale: f32) -> Option<Self> {
        if !scale.is_finite() || !(MIN_TIME_SCALE - 1e-4..=MAX_TIME_SCALE + 1e-4).contains(&scale) {
            return None;
        }
        // In range, so the rounded hundredths fit 10..=400.
        let hundredths = (scale * 100.0).round().clamp(10.0, 400.0);
        // Clamped to 10..=400 above, so the cast is exact.
        Some(Self(hundredths as u16))
    }

    /// `scale` clamped into range (the toolbar slider).
    #[must_use]
    pub fn clamped(scale: f32) -> Self {
        let scale = if scale.is_finite() { scale } else { 1.0 };
        Self::new(scale.clamp(MIN_TIME_SCALE, MAX_TIME_SCALE)).unwrap_or(Self::ONE)
    }

    /// The scale as a multiplier.
    #[must_use]
    pub fn get(self) -> f32 {
        f32::from(self.0) / 100.0
    }
}

impl Default for TimeScale {
    fn default() -> Self {
        Self::ONE
    }
}

/// The play world's mute state (a play-world resource; gone with the session).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PieAudioMute {
    /// Whether the play world's audio is muted.
    pub muted: bool,
    /// The root bus's own mute before PIE muted it, put back on unmute.
    previous: Option<bool>,
}

/// Agent and panel error codes of the start option.
pub mod codes {
    /// `start` is not `player_start`, `camera` or `entity`.
    pub const START_UNKNOWN: &str = "play_start_unknown";
    /// `start: entity` with no `entity` guid, or one that is not a player start of the edit world.
    pub const START_NOT_PLAYER_START: &str = "play_start_not_player_start";
    /// `time_scale` outside 0.1..=4.
    pub const TIME_SCALE_OUT_OF_RANGE: &str = "play_time_scale_out_of_range";
    /// `editor.play_controls` with no running session.
    pub const NOT_PLAYING: &str = "not_playing";
}

/// The level's player starts in `world`: `(guid, name)` in entity order (the Play dropdown lists
/// them when there are several). A start without a guid cannot be chosen and is left out.
#[must_use]
pub fn player_starts(world: &mut World) -> Vec<(Guid, String)> {
    let mut query = world
        .query_filtered::<(Entity, &EntityGuid, Option<&Name>), With<regia_framework::player::PlayerStart>>();
    let mut starts: Vec<(Entity, Guid, String)> = query
        .iter(world)
        .map(|(entity, guid, name)| {
            let label = name.map_or_else(|| "Player start".to_owned(), |n| n.as_str().to_owned());
            (entity, guid.0, label)
        })
        .collect();
    starts.sort_by_key(|(entity, _, _)| (entity.index().index(), entity.to_bits()));
    starts.into_iter().map(|(_, guid, name)| (guid, name)).collect()
}

/// Whether the edit world's entity `guid` is a player start.
#[must_use]
pub fn is_player_start(world: &World, guid: Guid) -> bool {
    crate::hierarchy_ops::guid_entity(world, guid).is_some_and(|entity| {
        world
            .get::<regia_framework::player::PlayerStart>(entity)
            .is_some()
    })
}

/// The editor camera's pose as a transform: at its eye, looking at its target.
#[must_use]
pub fn camera_pose(camera: &EditorCamera) -> Transform {
    let world_from_view = camera.view().inverse();
    let mut pose = Transform::from_affine(regia_core::math::Affine3A::from_mat4(world_from_view));
    pose.scale = regia_core::math::Vec3::ONE;
    pose
}

/// Name of the play-only player start the camera option spawns.
pub const CAMERA_START_NAME: &str = "pie start (editor camera)";

/// Applies `start` to the play world `play` after the level is spawned and before the game's
/// `on_load`: only the chosen start keeps its `PlayerStart` marker. `camera` is the editor camera
/// of the edit world.
///
/// # Errors
///
/// [`codes::START_NOT_PLAYER_START`] when `start` names a guid that is no player start of the copy.
pub fn apply_start(play: &mut World, start: PlayStart, camera: Option<&EditorCamera>) -> Result<(), String> {
    use regia_framework::player::PlayerStart;
    let keep = match start {
        PlayStart::PlayerStart => return Ok(()),
        PlayStart::Camera => None,
        PlayStart::Entity(guid) => {
            let found = play
                .query_filtered::<(Entity, &EntityGuid), With<PlayerStart>>()
                .iter(play)
                .find(|(_, g)| g.0 == guid)
                .map(|(entity, _)| entity);
            match found {
                Some(entity) => Some(entity),
                None => {
                    return Err(format!(
                        "{}: {guid} is not a player start of the level",
                        codes::START_NOT_PLAYER_START
                    ));
                }
            }
        }
    };
    let others: Vec<Entity> = play
        .query_filtered::<Entity, With<PlayerStart>>()
        .iter(play)
        .filter(|entity| Some(*entity) != keep)
        .collect();
    for entity in &others {
        play.entity_mut(*entity).remove::<PlayerStart>();
    }
    if start == PlayStart::Camera {
        let pose = camera.map_or_else(|| camera_pose(&EditorCamera::default()), camera_pose);
        play.spawn((
            Name::new(CAMERA_START_NAME),
            pose,
            GlobalTransform(pose.compute_affine()),
            PlayerStart,
        ));
    }
    tracing::info!(
        target: "regia_editor",
        start = start.as_str(),
        set_aside = others.len(),
        "play starts from the chosen start"
    );
    Ok(())
}

/// After the first frame of a session that started at the camera: when the play world has no
/// player, its window cameras move to the editor camera's pose ("places the play camera"). Answers
/// how many cameras moved.
pub fn place_cameras_without_player(play: &mut World, camera: Option<&EditorCamera>) -> usize {
    let has_player = play
        .query::<&regia_framework::player::Player>()
        .iter(play)
        .next()
        .is_some();
    if has_player {
        return 0;
    }
    let pose = camera.map_or_else(|| camera_pose(&EditorCamera::default()), camera_pose);
    let cameras: Vec<Entity> = play
        .query_filtered::<(Entity, &regia_render::Camera), Without<crate::pie::PiePlayerCamera>>()
        .iter(play)
        .filter(|(_, camera)| matches!(camera.target, regia_render::RenderTarget::Window))
        .map(|(entity, _)| entity)
        .collect();
    for entity in &cameras {
        // A child camera is detached from its parent so the pose is its world pose.
        play.entity_mut(*entity).remove::<ChildOf>();
        if let Some(mut transform) = play.get_mut::<Transform>(*entity) {
            *transform = pose;
        }
        if let Some(mut global) = play.get_mut::<GlobalTransform>(*entity) {
            *global = GlobalTransform(pose.compute_affine());
        }
    }
    cameras.len()
}

/// Sets the play world's gameplay time scale.
pub fn set_time_scale(play: &mut World, scale: TimeScale) {
    play.get_resource_or_insert_with(Time::default).time_scale = scale.get();
}

/// The play world's gameplay time scale (1 when it has no clock).
#[must_use]
pub fn time_scale(play: &World) -> f32 {
    play.get_resource::<Time>().map_or(1.0, |time| time.time_scale)
}

/// The root bus of the play world's audio, when it has one.
fn root_bus(audio: &regia_audio::Audio) -> Option<String> {
    audio
        .bus_layout()
        .buses
        .iter()
        .find(|bus| bus.parent.is_none())
        .map(|bus| bus.name.clone())
}

/// Mutes or unmutes the play world's audio and records it in [`PieAudioMute`].
pub fn set_mute(play: &mut World, muted: bool) {
    let mut state = play.get_resource::<PieAudioMute>().copied().unwrap_or_default();
    state.muted = muted;
    if !muted
        && let Some(mut audio) = play.get_resource_mut::<regia_audio::Audio>()
        && let Some(root) = root_bus(&audio)
    {
        let previous = state.previous.take().unwrap_or(false);
        if let Err(error) = audio.set_bus_mute(&root, previous) {
            tracing::warn!(target: "regia_editor", "play unmute: {error}");
        }
    }
    play.insert_resource(state);
    if muted {
        enforce_mute(play);
    }
}

/// Keeps a muted play world muted: a game that builds its audio after play started, or one that
/// unmutes its root bus, is muted again. Cheap when nothing changed; runs once per play frame.
pub fn enforce_mute(play: &mut World) {
    let Some(mut state) = play.get_resource::<PieAudioMute>().copied() else {
        return;
    };
    if !state.muted {
        return;
    }
    let Some(mut audio) = play.get_resource_mut::<regia_audio::Audio>() else {
        return;
    };
    let Some(root) = root_bus(&audio) else {
        return;
    };
    let current = audio.bus_muted(&root).unwrap_or(false);
    if current {
        return;
    }
    if state.previous.is_none() {
        state.previous = Some(current);
    }
    if let Err(error) = audio.set_bus_mute(&root, true) {
        tracing::warn!(target: "regia_editor", "play mute: {error}");
        return;
    }
    play.insert_resource(state);
}

/// Whether the play world's audio is muted by PIE.
#[must_use]
pub fn muted(play: &World) -> bool {
    play.get_resource::<PieAudioMute>()
        .is_some_and(|state| state.muted)
}

// ---------------------------------------------------------------------------------------------
// Toolbar: the start menu, the speed slider, the mute toggle and the play-mode tint.

/// Preference: tint the editor while a play session runs (ADR-7829). The colors are the theme's
/// `accents.play_mode`, `play_server` and `play_client_1..4` tokens.
pub const PREF_TINT: &str = "play.tint";

/// The start menu's label for `start`.
fn start_label(start: PlayStart, starts: &[(Guid, String)]) -> String {
    match start {
        PlayStart::PlayerStart => "From player start".to_owned(),
        PlayStart::Camera => "From editor camera".to_owned(),
        PlayStart::Entity(guid) => starts.iter().find(|(g, _)| *g == guid).map_or_else(
            || "From player start (gone)".to_owned(),
            |(_, name)| format!("From {name}"),
        ),
    }
}

/// The play toolbar's start menu (always), and while a session runs its speed slider and mute
/// toggle. Every control is a focusable egui widget (keyboard: Tab to it, Space/Enter to open or
/// toggle, arrows to move the slider); requests go through [`crate::pie::PieRequests`].
pub fn toolbar_controls(ui: &mut egui::Ui, world: &mut World, status: &crate::pie::PieStatus) {
    use crate::agent_parity::{
        PLAY_MUTE, PLAY_START_CAMERA, PLAY_START_ENTITY, PLAY_START_PLAYER_START, action_button_text,
        action_toggle,
    };
    let choice = world
        .get_resource::<PlayStartChoice>()
        .map(|c| c.0)
        .unwrap_or_default();
    let starts = player_starts(world);
    let mark = |on: bool| if on { "● " } else { "○ " };
    ui.menu_button(format!("⚑ {}", start_label(choice, &starts)), |ui| {
        let mut picked = None;
        if action_button_text(
            ui,
            &PLAY_START_PLAYER_START,
            format!("{}From player start", mark(choice == PlayStart::PlayerStart)),
        )
        .on_hover_text("The game's spawn rules pick the level's player start")
        .clicked()
        {
            picked = Some(PlayStart::PlayerStart);
        }
        if action_button_text(
            ui,
            &PLAY_START_CAMERA,
            format!("{}From editor camera", mark(choice == PlayStart::Camera)),
        )
        .on_hover_text(
            "The player spawns where the editor camera is; with no player, the play camera starts there",
        )
        .clicked()
        {
            picked = Some(PlayStart::Camera);
        }
        // A chosen start only means something when the level has several.
        if starts.len() > 1 {
            ui.separator();
            for (guid, name) in &starts {
                let on = choice == PlayStart::Entity(*guid);
                if action_button_text(ui, &PLAY_START_ENTITY, format!("{}From {name}", mark(on))).clicked() {
                    picked = Some(PlayStart::Entity(*guid));
                }
            }
        }
        if let Some(start) = picked {
            world.insert_resource(PlayStartChoice(start));
            ui.close();
        }
    });
    if status.mode.is_none() {
        return;
    }
    let queue = |world: &mut World, request: crate::pie::PieRequest| {
        world
            .get_resource_or_insert_with::<crate::pie::PieRequests>(Default::default)
            .push(request);
    };
    let mut scale = if status.time_scale > 0.0 {
        status.time_scale
    } else {
        1.0
    };
    let slider = ui
        .add(
            egui::Slider::new(&mut scale, MIN_TIME_SCALE..=MAX_TIME_SCALE)
                .logarithmic(true)
                .max_decimals(2)
                .suffix("×")
                .text("Speed"),
        )
        .on_hover_text("Play speed (0.1× to 4×; Pause stops time). Resets when play stops.");
    if slider.changed() {
        // An absolute value: a second UI pass in the same frame queues the same scale.
        queue(
            world,
            crate::pie::PieRequest::SetTimeScale(TimeScale::clamped(scale)),
        );
    }
    let mut muted = status.muted;
    if action_toggle(ui, &PLAY_MUTE, &mut muted)
        .on_hover_text(
            "Mute the game's audio (the editor's own sounds keep playing). Resets when play stops.",
        )
        .changed()
    {
        queue(world, crate::pie::PieRequest::SetMute(muted));
    }
    // The Match strip (ADR-8738): its controls call `editor.match_control`.
    ui.menu_button("⚔ Match", |ui| crate::match_rules_play::match_strip(ui, world))
        .response
        .on_hover_text("Start, pause, step and end the match; bot seats; the next playlist row");
}

/// The color the editor tints with while a session runs: the viewed network role's token in a
/// session with client worlds, else `accents.play_mode`. `None` while stopped or with the
/// [`PREF_TINT`] preference off.
#[must_use]
pub fn play_tint(world: &World) -> Option<egui::Color32> {
    let running = world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|status| status.mode.is_some());
    if !running || !tint_enabled(world) {
        return None;
    }
    let accents = &crate::theme::tokens(world).accents;
    Some(match crate::pie_worlds::viewed_role(world) {
        None => accents.play_mode,
        Some(crate::pie_worlds::PieViewRole::Server) => accents.play_server,
        Some(crate::pie_worlds::PieViewRole::Client(index)) => {
            accents.play_clients[index % accents.play_clients.len()]
        }
    })
}

/// Whether the [`PREF_TINT`] preference is on (the default).
#[must_use]
pub fn tint_enabled(world: &World) -> bool {
    world
        .get_resource::<crate::editor_prefs::EditorPreferences>()
        .is_none_or(|prefs| prefs.bool(PREF_TINT))
}

/// Paints the play toolbar's tint behind its widgets (call first inside the toolbar).
pub fn paint_toolbar_tint(ui: &egui::Ui, color: egui::Color32) {
    let rect = ui.max_rect().expand(ui.spacing().item_spacing.y);
    ui.painter().rect_filled(rect, 0.0, color.gamma_multiply(0.22));
    ui.painter().rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(PLAY_BORDER_WIDTH, color),
        egui::StrokeKind::Inside,
    );
}

/// Width of the play border, points (it scales with `appearance.ui_scale` like every point size).
pub const PLAY_BORDER_WIDTH: f32 = 3.0;

/// Paints the play border around `rect` (the play viewport).
pub fn paint_viewport_border(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(PLAY_BORDER_WIDTH, color),
        egui::StrokeKind::Inside,
    );
}

// ---------------------------------------------------------------------------------------------
// Agent surface: `editor.play { start, entity, time_scale, mute }`, `editor.play_controls`.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// `start` as `editor.play_state` answers it: `{ kind, entity? }`.
#[must_use]
pub fn start_json(start: PlayStart) -> Value {
    match start {
        PlayStart::Entity(guid) => json!({ "kind": start.as_str(), "entity": guid.to_string() }),
        _ => json!({ "kind": start.as_str() }),
    }
}

/// Parses `editor.play`'s `start`, `entity`, `time_scale` and `mute` into `options`.
///
/// # Errors
///
/// [`codes::START_UNKNOWN`], [`codes::START_NOT_PLAYER_START`], [`codes::TIME_SCALE_OUT_OF_RANGE`].
pub fn play_params(
    world: &World,
    params: &Value,
    options: &mut crate::pie::PlayOptions,
) -> Result<(), RpcError> {
    use crate::agent_ext::play_option_error;
    if let Some(start) = params.get("start").filter(|v| !v.is_null()) {
        options.start = Some(match start.as_str().unwrap_or("") {
            "player_start" => PlayStart::PlayerStart,
            "camera" => PlayStart::Camera,
            "entity" => {
                let text = params.get("entity").and_then(Value::as_str).unwrap_or("");
                let guid = text
                    .parse::<Guid>()
                    .ok()
                    .filter(|guid| is_player_start(world, *guid));
                PlayStart::Entity(guid.ok_or_else(|| {
                    play_option_error(
                        codes::START_NOT_PLAYER_START,
                        format!("`entity` must be the guid of a player start of the level, not {text:?}"),
                    )
                })?)
            }
            _ => {
                return Err(play_option_error(
                    codes::START_UNKNOWN,
                    format!("`start` must be \"player_start\", \"camera\" or \"entity\", not {start}"),
                ));
            }
        });
    }
    if let Some(scale) = params.get("time_scale").filter(|v| !v.is_null()) {
        options.time_scale = time_scale_param(scale)?;
    }
    if let Some(mute) = params.get("mute").and_then(Value::as_bool) {
        options.muted = mute;
    }
    Ok(())
}

/// A `time_scale` parameter.
fn time_scale_param(scale: &Value) -> Result<TimeScale, RpcError> {
    // A scale beyond f32 is out of range anyway.
    scale.as_f64().and_then(|v| TimeScale::new(v as f32)).ok_or_else(|| {
        crate::agent_ext::play_option_error(
            codes::TIME_SCALE_OUT_OF_RANGE,
            format!("`time_scale` must be a number {MIN_TIME_SCALE}..={MAX_TIME_SCALE} (pause with editor.pause), not {scale}"),
        )
    })
}

/// Adds `start`, `time_scale` and `mute` to `editor.play`'s answer.
pub fn play_answer(world: &World, options: &crate::pie::PlayOptions, answer: &mut Value) {
    let start = options
        .start
        .or_else(|| world.get_resource::<PlayStartChoice>().map(|choice| choice.0))
        .unwrap_or_default();
    answer["start"] = start_json(start);
    answer["time_scale"] = json!(options.time_scale.get());
    answer["mute"] = json!(options.muted);
}

/// `editor.play_controls`: the running session's time scale, mute and device controls.
fn play_controls(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let status = world
        .get_resource::<crate::pie::PieStatus>()
        .cloned()
        .unwrap_or_default();
    if status.mode.is_none() {
        return Err(crate::agent_ext::play_option_error(
            codes::NOT_PLAYING,
            "play controls need a running play session (editor.play first)",
        ));
    }
    let scale = params
        .get("time_scale")
        .filter(|v| !v.is_null())
        .map(time_scale_param)
        .transpose()?;
    let mute = params.get("mute").and_then(Value::as_bool);
    let device = crate::play_device::control_param(world, params)?;
    let device_changed = device.is_some_and(|control| crate::play_device::apply_control(world, control));
    let mut requests = world.get_resource_or_insert_with::<crate::pie::PieRequests>(Default::default);
    if let Some(scale) = scale {
        requests.push(crate::pie::PieRequest::SetTimeScale(scale));
    }
    if let Some(mute) = mute {
        requests.push(crate::pie::PieRequest::SetMute(mute));
    }
    Ok(json!({
        "queued": scale.is_some() || mute.is_some() || device_changed,
        "time_scale": scale.map_or(status.time_scale, TimeScale::get),
        "mute": mute.unwrap_or(status.muted),
        "hint": "poll editor.play_state: the editor applies the request on its next frame",
    }))
}

/// `editor.play_controls` (ADR-7829).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.play_controls",
            tool: "play_controls",
            description: "Change the running play session's gameplay time scale (0.1..=4; pause is \
                editor.pause) and mute its audio (the play world's root bus; the editor's own sounds \
                are not affected). Both reset when play stops. Without a session: `not_playing`; a \
                scale out of range: `play_time_scale_out_of_range`. Applied on the next editor frame. \
                Also accepts live play-window device controls: a safe-area outline and virtual pad input.",
            params: &[
                ParamSchema {
                    name: "time_scale",
                    kind: "number",
                    required: false,
                    description: "Gameplay speed multiplier, 0.1..=4.",
                },
                ParamSchema {
                    name: "mute",
                    kind: "boolean",
                    required: false,
                    description: "Mute (true) or unmute (false) the play world's audio.",
                },
                ParamSchema {
                    name: "device",
                    kind: "object",
                    required: false,
                    description: "Live play-window controls: { safe_area_outline?: bool, button?: dpad_up|dpad_down|dpad_left|dpad_right|north|south|west|east|select|start, pressed?: bool }. Button and pressed must be supplied together, with the virtual gamepad enabled. The outline changes immediately; a pad transition is queued for this play world. Unknown button, invalid fields, disabled pad or a full input queue: invalid_params. No open play window: play_device_needs_window.",
                },
            ],
            result: "{ queued, time_scale, mute, hint }",
            mutating: true,
            permission: Permission::Play,
        },
        play_controls as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_scale_accepts_the_slider_range_and_refuses_the_rest() {
        assert_eq!(TimeScale::new(1.0), Some(TimeScale::ONE));
        assert_eq!(TimeScale::new(0.1).map(TimeScale::get), Some(0.1));
        assert_eq!(TimeScale::new(4.0).map(TimeScale::get), Some(4.0));
        assert_eq!(TimeScale::new(0.0), None, "0 is the Pause button");
        assert_eq!(TimeScale::new(4.5), None);
        assert_eq!(TimeScale::new(f32::NAN), None);
        assert_eq!(TimeScale::clamped(9.0).get(), 4.0);
        assert_eq!(TimeScale::clamped(f32::INFINITY), TimeScale::ONE);
    }

    #[test]
    fn camera_pose_sits_at_the_eye_and_looks_at_the_target() {
        let camera = EditorCamera::default();
        let pose = camera_pose(&camera);
        assert!((pose.translation - camera.eye()).length() < 1e-4, "{pose:?}");
        // A transform looks along its local −Z.
        let forward = pose.rotation * regia_core::math::Vec3::NEG_Z;
        assert!((forward - camera.forward()).length() < 1e-3, "{forward:?}");
    }

    #[test]
    fn the_entity_start_keeps_only_the_chosen_marker() {
        use regia_framework::player::PlayerStart;
        let mut world = World::new();
        let a = Guid::new_random();
        let b = Guid::new_random();
        world.spawn((EntityGuid(a), Transform::default(), PlayerStart));
        world.spawn((EntityGuid(b), Transform::default(), PlayerStart));
        apply_start(&mut world, PlayStart::Entity(b), None).expect("start");
        let left: Vec<Guid> = world
            .query_filtered::<&EntityGuid, With<PlayerStart>>()
            .iter(&world)
            .map(|g| g.0)
            .collect();
        assert_eq!(left, vec![b]);
        let missing = apply_start(&mut world, PlayStart::Entity(a), None).expect_err("a lost its marker");
        assert!(missing.starts_with(codes::START_NOT_PLAYER_START), "{missing}");
    }

    /// Keyboard path (accessibility bar): Tab reaches the Mute toggle and Space queues the mute; the
    /// start menu opens from the keyboard and Enter on an entry chooses it.
    #[test]
    fn the_toolbar_controls_work_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let ctx = egui::Context::default();
        let size = [1200.0, 200.0];
        let mut world = World::new();
        let status = crate::pie::PieStatus {
            mode: Some(crate::pie::PlayMode::Playing),
            time_scale: 1.0,
            ..Default::default()
        };
        let mut draw = |ui: &mut egui::Ui| {
            ui.horizontal(|ui| toolbar_controls(ui, &mut world, &status));
        };
        tab_to(&ctx, size, "Mute", false, 12, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Space, egui::Modifiers::NONE)],
            &mut draw,
        );
        drop(draw);
        let queued = world
            .get_resource::<crate::pie::PieRequests>()
            .map(|r| r.queue.clone())
            .unwrap_or_default();
        assert!(
            queued.contains(&crate::pie::PieRequest::SetMute(true)),
            "{queued:?}"
        );

        // The start menu: Tab to it, Enter opens it, Tab to an entry, Enter picks it.
        let ctx = egui::Context::default();
        let mut world = World::new();
        let stopped = crate::pie::PieStatus::default();
        let mut draw = |ui: &mut egui::Ui| {
            ui.horizontal(|ui| toolbar_controls(ui, &mut world, &stopped));
        };
        tab_to(&ctx, size, "⚑ From player start", false, 6, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, size, vec![], &mut draw);
        tab_to(&ctx, size, "○ From editor camera", false, 6, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        drop(draw);
        assert_eq!(
            world.get_resource::<PlayStartChoice>(),
            Some(&PlayStartChoice(PlayStart::Camera))
        );
    }

    #[test]
    fn the_tint_follows_the_session_and_the_preference() {
        let mut world = World::new();
        assert_eq!(play_tint(&world), None, "stopped");
        world.insert_resource(crate::pie::PieStatus {
            mode: Some(crate::pie::PlayMode::Paused),
            ..Default::default()
        });
        let theme = crate::theme::ThemeTokens::default();
        assert_eq!(
            play_tint(&world),
            Some(theme.accents.play_mode),
            "the theme's play_mode token"
        );
        let mut prefs = crate::editor_prefs::EditorPreferences::default();
        prefs
            .set(PREF_TINT, &crate::editor_prefs::PrefValue::Bool(false))
            .expect("pref");
        world.insert_resource(prefs);
        assert_eq!(play_tint(&world), None, "the preference turns it off");
    }

    #[test]
    fn mute_without_audio_is_recorded_and_unmute_clears_it() {
        let mut world = World::new();
        set_mute(&mut world, true);
        assert!(muted(&world));
        set_mute(&mut world, false);
        assert!(!muted(&world));
    }
}
