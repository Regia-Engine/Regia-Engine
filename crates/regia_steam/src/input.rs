// SPDX-License-Identifier: MPL-2.0
//! Steam Input glue (specs/24 § 2 "Steam Input", ADR-350): the action-set mapping between
//! Regia Engine's input contexts and Steam Input, the in-game actions manifest generator, and the
//! `PreUpdate` bridge that feeds Steam Input action states into Regia Engine's input pipeline.
//!
//! What maps to what:
//!
//! - A Regia Engine **input context** (`InputConfig::contexts`) is a Steam Input **action set**
//!   ([`action_set_name`]). Whichever enabled context has the highest priority for the player
//!   decides the active set ([`active_context`]); the set is re-activated when it changes or a
//!   controller connects.
//! - A Regia Engine **action** with at least one gamepad binding in a context becomes a Steam Input
//!   action of that set, named `<context>_<action>` ([`action_name`]), so the same Regia Engine action
//!   in two contexts never shares a Steam action name. `Button` is a digital action, `Axis1D` an
//!   analog trigger, `Axis2D` an analog stick. `Axis3D` has no Steam Input equivalent and is
//!   rejected by [`action_manifest`].
//! - Every set also carries the eight fixed **UI actions** ([`UI_ACTIONS`]). The bridge turns
//!   their edges into the standard-layout `GamepadButtonChanged` messages that
//!   `regia_ui::focus::ui_gamepad_navigation` already reads (D-pad, A, B, shoulders), so
//!   controller navigation (ADR-213) keeps working when Steam Input hands the game actions
//!   instead of a virtual gamepad.
//! - Game actions are written with `InputInjection::hold` for as long as Steam reports a
//!   non-zero value, and released when it goes back to zero, so keyboard and mouse bindings of the
//!   same action keep working.
//!
//! Steam Input is opt-in ([`SteamInputSettings::enabled`], default off): without it nothing here
//! runs and the `gilrs` path is unchanged. Not done: action set layers (the `steamworks` 0.13
//! bindings do not expose them), several players on several controllers (player 0 reads the
//! first controller), and Steam's own per-origin glyphs for prompts (`deck_glyphs` labels the
//! standard layout instead). A suspend or resume releases everything the bridge holds and starts
//! a fresh Steam Input session (ADR-526).

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_input::{ActionKind, DeviceClass, InputConfig, InputContext, InputContexts, InputInjection};
use regia_platform::{AppLifecycle, ButtonState, GamepadButton, GamepadButtonChanged, LifecycleEvent};

use crate::backend::SteamBackend;
use crate::resource::Steam;

/// The device id the bridge uses for the `GamepadButtonChanged` messages it synthesizes from the
/// UI actions (the bytes spell `STIP`).
pub const STEAM_INPUT_PAD_DEVICE: u64 = 0x5354_4950;

/// The fixed UI navigation actions every action set carries, and the standard-layout gamepad
/// button each one is reported as. The Steam action name is `<context>_<suffix>`.
pub const UI_ACTIONS: [(&str, GamepadButton); 8] = [
    ("ui_up", GamepadButton::DPadUp),
    ("ui_down", GamepadButton::DPadDown),
    ("ui_left", GamepadButton::DPadLeft),
    ("ui_right", GamepadButton::DPadRight),
    ("ui_accept", GamepadButton::South),
    ("ui_cancel", GamepadButton::East),
    ("ui_prev", GamepadButton::LeftShoulder),
    ("ui_next", GamepadButton::RightShoulder),
];

/// Steam Input's action set name for a Regia Engine context: upper-case ASCII letters and digits,
/// everything else turned into `_` — the same folding [`action_name`] applies to action names, so
/// the generated VDF never holds a non-ASCII set name Steam's loader might treat differently than
/// the activation lookup.
pub fn action_set_name(context: &str) -> String {
    context
        .chars()
        .map(|c| {
            if c.is_ascii_alphabetic() {
                c.to_ascii_uppercase()
            } else if c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Lower-cases `name` and turns everything that is not an ASCII letter or digit into `_`.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The Steam Input action name for Regia Engine action `action` in `context`: `<context>_<action>`,
/// lower-case ASCII letters, digits and `_` only (the UI actions use the same scheme with the
/// [`UI_ACTIONS`] suffixes).
pub fn action_name(context: &str, action: &str) -> String {
    format!("{}_{}", sanitize(context), sanitize(action))
}

/// The enabled context that decides the Steam Input action set for `player`: the highest
/// priority among the enabled contexts the config declares (ties go to the name that sorts
/// first). `None` when no enabled context exists in the config.
pub fn active_context<'a>(
    config: &'a InputConfig,
    contexts: &'a InputContexts,
    player: u8,
) -> Option<&'a str> {
    contexts
        .enabled(player)
        .iter()
        .filter_map(|name| config.contexts.get(name).map(|context| (context.priority, name)))
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))
        .map(|(_, name)| name.as_str())
}

/// The actions of `context` that have at least one gamepad binding, with their kinds.
fn gamepad_actions(context: &InputContext, config: &InputConfig) -> BTreeMap<String, ActionKind> {
    context
        .bindings
        .iter()
        .filter(|binding| binding.input.class() == DeviceClass::Gamepad)
        .filter_map(|binding| {
            config
                .kind(&binding.action)
                .map(|kind| (binding.action.clone(), kind))
        })
        .collect()
}

/// Why [`action_manifest`] could not describe the config to Steam Input.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// A gamepad-bound action is `Axis3D`, which Steam Input cannot express.
    #[error("action `{action}` in context `{context}` is a 3D action; Steam Input has no 3D action type")]
    Unsupported {
        /// The context holding the binding.
        context: String,
        /// The action name.
        action: String,
    },
    /// Two actions of one context map to the same Steam action name (for example `Jump` and
    /// `jump`, or an action called `ui_up`), or two contexts map to the same action set name
    /// (`Main Menu` and `Main_Menu`; then `first` and `second` are the context names).
    #[error("context `{context}`: `{first}` and `{second}` both map to the Steam action `{name}`")]
    NameCollision {
        /// The context holding both actions.
        context: String,
        /// The colliding Steam action name.
        name: String,
        /// The first action that produced it.
        first: String,
        /// The second action that produced it.
        second: String,
    },
    /// The input config itself could not be read ([`action_manifest_from_ron`]).
    #[error("{0}")]
    Config(String),
}

/// One action set while the manifest is being built.
struct SetPlan {
    context: String,
    sticks: Vec<(String, String)>,
    triggers: Vec<(String, String)>,
    buttons: Vec<(String, String)>,
}

/// Escapes a string for a double-quoted Valve text (VDF) value.
fn vdf(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn plan_set(
    context_name: &str,
    context: &InputContext,
    config: &InputConfig,
) -> Result<SetPlan, ManifestError> {
    let mut plan = SetPlan {
        context: context_name.to_owned(),
        sticks: Vec::new(),
        triggers: Vec::new(),
        buttons: Vec::new(),
    };
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut claim = |name: &str, label: &str| -> Result<(), ManifestError> {
        match seen.insert(name.to_owned(), label.to_owned()) {
            Some(first) => Err(ManifestError::NameCollision {
                context: context_name.to_owned(),
                name: name.to_owned(),
                first,
                second: label.to_owned(),
            }),
            None => Ok(()),
        }
    };
    for (action, kind) in gamepad_actions(context, config) {
        let name = action_name(context_name, &action);
        claim(&name, &action)?;
        let entry = (name, action.clone());
        match kind {
            ActionKind::Button => plan.buttons.push(entry),
            ActionKind::Axis1D => plan.triggers.push(entry),
            ActionKind::Axis2D => plan.sticks.push(entry),
            ActionKind::Axis3D => {
                return Err(ManifestError::Unsupported {
                    context: context_name.to_owned(),
                    action,
                });
            }
        }
    }
    for (suffix, _) in UI_ACTIONS {
        let name = action_name(context_name, suffix);
        claim(&name, suffix)?;
        plan.buttons.push((name, suffix.to_owned()));
    }
    Ok(plan)
}

/// Writes one `"Kind" { "<name>" { "title" "#Action_<name>" [mode] } ... }` section.
fn write_section(out: &mut String, kind: &str, mode: Option<&str>, entries: &[(String, String)]) {
    if entries.is_empty() {
        return;
    }
    out.push_str(&format!("\t\t\t{}\n\t\t\t{{\n", vdf(kind)));
    for (name, _) in entries {
        out.push_str(&format!("\t\t\t\t{}\n\t\t\t\t{{\n", vdf(name)));
        out.push_str(&format!(
            "\t\t\t\t\t\"title\"\t{}\n",
            vdf(&format!("#Action_{name}"))
        ));
        if let Some(mode) = mode {
            out.push_str(&format!("\t\t\t\t\t\"input_mode\"\t{}\n", vdf(mode)));
        }
        out.push_str("\t\t\t\t}\n");
    }
    out.push_str("\t\t\t}\n");
}

/// Generates Steam Input's in-game actions file (Valve text format, `game_actions_<appid>.vdf`)
/// from the project's input config: one action set per context, each with its gamepad-bound
/// actions and the [`UI_ACTIONS`], plus the English localization of titles. Deterministic: the
/// same config always yields the same text.
///
/// # Errors
///
/// [`ManifestError`] when a gamepad-bound action cannot be expressed or two names collide.
pub fn action_manifest(config: &InputConfig) -> Result<String, ManifestError> {
    let mut plans = Vec::new();
    let mut sets: BTreeMap<String, &str> = BTreeMap::new();
    for (name, context) in &config.contexts {
        // Two contexts that upper-case to one set name would share a VDF key and lose actions.
        let set = action_set_name(name);
        if let Some(first) = sets.insert(set.clone(), name.as_str()) {
            return Err(ManifestError::NameCollision {
                context: name.clone(),
                name: set,
                first: first.to_owned(),
                second: name.clone(),
            });
        }
        plans.push(plan_set(name, context, config)?);
    }

    let mut out = String::from("\"Action Manifest\"\n{\n\t\"actions\"\n\t{\n");
    for plan in &plans {
        let set = action_set_name(&plan.context);
        out.push_str(&format!("\t\t{}\n\t\t{{\n", vdf(&set)));
        out.push_str(&format!("\t\t\t\"title\"\t{}\n", vdf(&format!("#Set_{set}"))));
        write_section(&mut out, "StickPadGyro", Some("joystick_move"), &plan.sticks);
        write_section(&mut out, "AnalogTrigger", Some("trigger"), &plan.triggers);
        write_section(&mut out, "Button", None, &plan.buttons);
        out.push_str("\t\t}\n");
    }
    out.push_str("\t}\n\t\"localization\"\n\t{\n\t\t\"english\"\n\t\t{\n");
    for plan in &plans {
        let set = action_set_name(&plan.context);
        out.push_str(&format!(
            "\t\t\t{}\t{}\n",
            vdf(&format!("Set_{set}")),
            vdf(&plan.context)
        ));
        for (name, label) in plan.sticks.iter().chain(&plan.triggers).chain(&plan.buttons) {
            out.push_str(&format!(
                "\t\t\t{}\t{}\n",
                vdf(&format!("Action_{name}")),
                vdf(label)
            ));
        }
    }
    out.push_str("\t\t}\n\t}\n}\n");
    Ok(out)
}

/// Project settings for the Steam Input bridge. Insert before [`crate::SteamPlugin`] runs, or
/// change at runtime.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct SteamInputSettings {
    /// Whether the bridge reads Steam Input. Off by default: the `gilrs` gamepad path stays the
    /// only controller source, exactly as before.
    pub enabled: bool,
    /// The Regia Engine player whose contexts choose the action set and whose actions receive the
    /// values (default 0).
    pub player: u8,
}

/// What the bridge did last frame (read by the `steam.input` agent method and by tests).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct SteamInputStatus {
    /// The bridge is enabled, Steam is ready and Steam Input initialized.
    pub active: bool,
    /// Controllers Steam Input reported.
    pub controllers: usize,
    /// The Steam action set last chosen (an upper-case [`action_set_name`]).
    pub action_set: Option<String>,
    /// Regia Engine actions currently held at a non-zero value from Steam Input.
    pub driven_actions: Vec<String>,
    /// UI action suffixes (from [`UI_ACTIONS`]) currently down.
    pub ui_down: Vec<String>,
}

/// One gamepad action whose Steam name and handle were resolved for the current context.
#[derive(Debug)]
struct CachedAction {
    action: String,
    kind: ActionKind,
    steam_name: String,
    /// `input_*_handle` has been asked. `false` means the backend has no handle cache and the
    /// name is read every frame.
    resolved: bool,
    /// `Some(0)` is a resolved miss. Any other value is a handle.
    handle: Option<u64>,
    last: Vec3,
}

/// One UI action resolved the same way as `CachedAction`.
#[derive(Debug)]
struct CachedUi {
    steam_name: String,
    resolved: bool,
    handle: Option<u64>,
}

/// The bridge's frame-to-frame state.
#[derive(Resource, Debug, Default)]
pub(crate) struct SteamInputRuntime {
    initialized: bool,
    init_failed: bool,
    controllers: usize,
    set: Option<String>,
    /// The last activation was rejected; try again next frame.
    activation_failed: bool,
    held: BTreeSet<String>,
    ui_down: [bool; UI_ACTIONS.len()],
    /// Context the action cache was built for. Compared as text so a quiet frame does not
    /// allocate a new context name.
    cached_for: Option<String>,
    actions: Vec<CachedAction>,
    ui: Vec<CachedUi>,
}

/// Marks every action and UI slot whose handle resolved to `0` ("absent") as unresolved, so the
/// next read asks the backend again.
fn forget_resolved_misses(rt: &mut SteamInputRuntime) {
    for slot in &mut rt.actions {
        if slot.handle == Some(0) {
            slot.resolved = false;
            slot.handle = None;
        }
    }
    for slot in &mut rt.ui {
        if slot.handle == Some(0) {
            slot.resolved = false;
            slot.handle = None;
        }
    }
}

/// Reads a digital action, resolving its handle once per cache entry.
fn cached_digital(
    backend: &dyn SteamBackend,
    resolved: &mut bool,
    handle: &mut Option<u64>,
    name: &str,
) -> Option<bool> {
    if !*resolved {
        match backend.input_digital_handle(name) {
            None => return backend.input_digital(name),
            Some(found) => {
                *resolved = true;
                *handle = Some(found);
            }
        }
    }
    match *handle {
        Some(0) | None => None,
        Some(found) => backend.input_read_digital(found),
    }
}

/// Reads an analog action, resolving its handle once per cache entry.
fn cached_analog(
    backend: &dyn SteamBackend,
    resolved: &mut bool,
    handle: &mut Option<u64>,
    name: &str,
) -> Option<(f32, f32)> {
    if !*resolved {
        match backend.input_analog_handle(name) {
            None => return backend.input_analog(name),
            Some(found) => {
                *resolved = true;
                *handle = Some(found);
            }
        }
    }
    match *handle {
        Some(0) | None => None,
        Some(found) => backend.input_read_analog(found),
    }
}

fn cached_action_value(backend: &dyn SteamBackend, slot: &mut CachedAction) -> Vec3 {
    let sample = match slot.kind {
        ActionKind::Button => cached_digital(backend, &mut slot.resolved, &mut slot.handle, &slot.steam_name)
            .map(|down| if down { Vec3::X } else { Vec3::ZERO }),
        ActionKind::Axis1D => cached_analog(backend, &mut slot.resolved, &mut slot.handle, &slot.steam_name)
            .map(|(x, _)| Vec3::new(x, 0.0, 0.0)),
        ActionKind::Axis2D => cached_analog(backend, &mut slot.resolved, &mut slot.handle, &slot.steam_name)
            .map(|(x, y)| Vec3::new(x, y, 0.0)),
        ActionKind::Axis3D => None,
    };
    sample.unwrap_or(Vec3::ZERO)
}

/// The message for UI action `index` changing to `down`.
fn ui_message(index: usize, down: bool) -> GamepadButtonChanged {
    GamepadButtonChanged {
        device: STEAM_INPUT_PAD_DEVICE,
        button: UI_ACTIONS[index].1,
        state: if down {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        },
        value: if down { 1.0 } else { 0.0 },
    }
}

/// Releases everything the bridge holds and returns the release messages for pressed UI actions.
fn release_all(
    rt: &mut SteamInputRuntime,
    injection: Option<&mut InputInjection>,
) -> Vec<GamepadButtonChanged> {
    if let Some(injection) = injection {
        for action in &rt.held {
            injection.release(action);
        }
    }
    let released = (0..UI_ACTIONS.len())
        .filter(|&i| rt.ui_down[i])
        .map(|i| ui_message(i, false))
        .collect();
    *rt = SteamInputRuntime::default();
    released
}

/// `PreUpdate`, after the Steam pump and before action evaluation: chooses and activates the
/// action set, reads the actions of that set, holds them on `InputInjection` and reports the UI
/// actions as gamepad button messages. Action names are formatted and resolved once per context;
/// a quiet frame reads cached handles and skips `hold` when the value did not change. A no-op
/// while [`SteamInputSettings::enabled`] is off.
// ECS systems take one parameter per resource/message they touch (as `pump_steam` does); eight is
// this bridge's honest signature.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip_all, target = "regia_steam", name = "steam_input")]
pub(crate) fn steam_input_system(
    steam: Res<Steam>,
    settings: Res<SteamInputSettings>,
    config: Option<Res<InputConfig>>,
    contexts: Option<Res<InputContexts>>,
    mut injection: Option<ResMut<InputInjection>>,
    mut status: ResMut<SteamInputStatus>,
    mut rt: ResMut<SteamInputRuntime>,
    mut buttons: MessageWriter<GamepadButtonChanged>,
) {
    if !settings.enabled || !steam.is_ready() || rt.init_failed {
        if rt.initialized {
            for message in release_all(&mut rt, injection.as_deref_mut()) {
                buttons.write(message);
            }
        }
        if *status != SteamInputStatus::default() {
            *status = SteamInputStatus::default();
        }
        return;
    }
    if !rt.initialized {
        if !steam.backend.input_init() {
            tracing::warn!(target: "regia_steam", "Steam Input did not initialize; staying on the gamepad path");
            rt.init_failed = true;
            return;
        }
        rt.initialized = true;
    }

    let controllers = steam.backend.input_connected();
    let context_name = match (config.as_deref(), contexts.as_deref()) {
        (Some(config), Some(contexts)) => active_context(config, contexts, settings.player),
        _ => None,
    };
    let set_changed = context_name != rt.set.as_deref();
    let config_changed = config.as_ref().is_some_and(|config| config.is_changed());
    if set_changed || controllers != rt.controllers || rt.activation_failed {
        let mut failed = false;
        if let Some(name) = context_name
            && controllers > 0
        {
            let set = action_set_name(name);
            if let Err(err) = steam.backend.input_activate_action_set(&set) {
                // Retried every frame (the manifest may not be in Steam yet); logged once.
                if !rt.activation_failed {
                    tracing::warn!(target: "regia_steam", set = %set, "cannot activate the Steam Input action set: {err}");
                }
                failed = true;
            }
        }
        // Steam resolves action handles to `0` until the action manifest is loaded, the same
        // window in which the activation above is rejected. A miss cached in that window (or
        // before this controller connected) is asked again after an activation went through, so
        // the actions do not stay dead for the session; real misses stay cheap afterwards.
        if !failed && (rt.activation_failed || controllers != rt.controllers) {
            forget_resolved_misses(&mut rt);
        }
        rt.activation_failed = failed;
        if set_changed {
            rt.set = context_name.map(str::to_owned);
        }
        rt.controllers = controllers;
    }
    // A context or config change rebuilds the cache. Actions the previous set held are
    // released below, then the new set holds only what it still drives.
    let cache_rebuilt = rt.cached_for.as_deref() != context_name || config_changed;
    if cache_rebuilt {
        rt.actions.clear();
        rt.ui.clear();
        rt.cached_for = context_name.map(str::to_owned);
        if let (Some(name), Some(config)) = (context_name, config.as_deref()) {
            if let Some(context) = config.contexts.get(name) {
                for (action, kind) in gamepad_actions(context, config) {
                    rt.actions.push(CachedAction {
                        steam_name: action_name(name, &action),
                        action,
                        kind,
                        resolved: false,
                        handle: None,
                        last: Vec3::ZERO,
                    });
                }
            }
            for (suffix, _) in UI_ACTIONS {
                rt.ui.push(CachedUi {
                    steam_name: action_name(name, suffix),
                    resolved: false,
                    handle: None,
                });
            }
        }
    }

    let backend = steam.backend.as_ref();
    let rt = &mut *rt;
    let mut held_dirty = false;
    if cache_rebuilt && !rt.held.is_empty() {
        if let Some(injection) = injection.as_deref_mut() {
            for action in &rt.held {
                injection.release(action);
            }
        }
        rt.held.clear();
        held_dirty = true;
    }
    if controllers == 0 {
        if !rt.held.is_empty() {
            if let Some(injection) = injection.as_deref_mut() {
                for action in &rt.held {
                    injection.release(action);
                }
            }
            rt.held.clear();
            held_dirty = true;
        }
        for slot in &mut rt.actions {
            slot.last = Vec3::ZERO;
        }
    } else {
        let actions = &mut rt.actions;
        let held = &mut rt.held;
        for slot in actions.iter_mut() {
            let value = cached_action_value(backend, slot);
            // A non-finite axis (a misbehaving driver) reads as "not held", like an inactive
            // action: holding it would drive gameplay with NaN/inf every frame.
            let value = if value.is_finite() { value } else { Vec3::ZERO };
            if value == slot.last {
                continue;
            }
            if let Some(injection) = injection.as_deref_mut() {
                if value == Vec3::ZERO {
                    injection.release(&slot.action);
                } else {
                    injection.hold(slot.action.clone(), value);
                }
            }
            if value == Vec3::ZERO {
                held.remove(&slot.action);
            } else {
                held.insert(slot.action.clone());
            }
            slot.last = value;
            held_dirty = true;
        }
    }

    let mut ui_dirty = false;
    let ui = &mut rt.ui;
    let ui_down = &mut rt.ui_down;
    // Walk every UI action, not just the cached slots: with no active context the cache holds no
    // UI slots, and a button that was down when the context went away must still be released.
    for (index, was_down) in ui_down.iter_mut().enumerate() {
        let down = controllers > 0
            && ui.get_mut(index).is_some_and(|slot| {
                cached_digital(backend, &mut slot.resolved, &mut slot.handle, &slot.steam_name) == Some(true)
            });
        if down != *was_down {
            *was_down = down;
            buttons.write(ui_message(index, down));
            ui_dirty = true;
        }
    }

    if held_dirty || ui_dirty || set_changed || !status.active || status.controllers != controllers {
        *status = SteamInputStatus {
            active: true,
            controllers,
            action_set: rt.set.as_deref().map(action_set_name),
            driven_actions: rt.held.iter().cloned().collect(),
            ui_down: UI_ACTIONS
                .iter()
                .zip(rt.ui_down)
                .filter(|(_, down)| *down)
                .map(|((suffix, _), _)| (*suffix).to_owned())
                .collect(),
        };
    }
}

/// `Lifecycle`: on a suspend or a resume (on the Deck, also the synthetic pair the runner raises
/// after a sleep, ADR-213) releases every action the bridge holds and every UI button it reported
/// down, and forgets the Steam Input session, so the next frame initializes Steam Input again and
/// re-activates the action set (ADR-526). Without this a button held while the Deck went to sleep
/// stays held after it wakes, and a controller handle from before the sleep keeps the old set.
pub(crate) fn steam_input_on_lifecycle(
    lifecycle: Res<AppLifecycle>,
    mut injection: Option<ResMut<InputInjection>>,
    mut status: ResMut<SteamInputStatus>,
    mut rt: ResMut<SteamInputRuntime>,
    mut buttons: MessageWriter<GamepadButtonChanged>,
) {
    if !matches!(
        lifecycle.last_event,
        Some(LifecycleEvent::Suspended | LifecycleEvent::Resumed)
    ) {
        return;
    }
    if rt.initialized || rt.init_failed {
        for message in release_all(&mut rt, injection.as_deref_mut()) {
            buttons.write(message);
        }
    }
    if *status != SteamInputStatus::default() {
        *status = SteamInputStatus::default();
    }
}

/// The file name Steam expects for an app's in-game actions file: `game_actions_<appid>.vdf`.
pub fn action_manifest_file_name(app_id: u32) -> String {
    format!("game_actions_{app_id}.vdf")
}

/// [`action_manifest`] for an input config in RON (the text of `config/input.ron`), for tools
/// that do not link `regia_input` themselves (`regia steam input-manifest`).
///
/// # Errors
///
/// [`ManifestError::Config`] when the RON does not parse, else as [`action_manifest`].
pub fn action_manifest_from_ron(ron: &str) -> Result<String, ManifestError> {
    let config = InputConfig::from_ron(ron).map_err(ManifestError::Config)?;
    action_manifest(&config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_input::{Binding, InputSource, Stick};

    fn config() -> InputConfig {
        InputConfig::new()
            .action("Jump", ActionKind::Button)
            .action("Move", ActionKind::Axis2D)
            .action("Throttle", ActionKind::Axis1D)
            .action("Confirm", ActionKind::Button)
            .context("Gameplay", InputContext::new(0))
            .context("Menu", InputContext::consuming(10))
            .binding_in(
                "Gameplay",
                Binding::new("Jump", InputSource::gamepad_button(GamepadButton::South)),
            )
            .binding_in("Gameplay", Binding::new("Jump", InputSource::key("Space")))
            .binding_in(
                "Gameplay",
                Binding::new("Move", InputSource::gamepad_stick(Stick::Left)),
            )
            .binding_in(
                "Gameplay",
                Binding::new(
                    "Throttle",
                    InputSource::gamepad_axis(regia_platform::GamepadAxis::RightTrigger),
                ),
            )
            .binding_in(
                "Menu",
                Binding::new("Confirm", InputSource::gamepad_button(GamepadButton::South)),
            )
    }

    #[test]
    fn context_names_map_to_action_set_names() {
        assert_eq!(action_set_name("Gameplay"), "GAMEPLAY");
        assert_eq!(action_set_name("main menu"), "MAIN_MENU");
        assert_eq!(action_set_name("vehicle-racing"), "VEHICLE_RACING");
        // ASCII folding, like action names: no non-ASCII set name reaches the VDF (audit G,
        // finding 5).
        assert_eq!(action_set_name("Menü"), "MEN_");
        assert!(
            action_set_name("Straße").is_ascii(),
            "{:?}",
            action_set_name("Straße")
        );
    }

    #[test]
    fn action_names_are_scoped_to_their_context() {
        assert_eq!(action_name("Gameplay", "Jump"), "gameplay_jump");
        assert_eq!(action_name("Main Menu", "Go-Back"), "main_menu_go_back");
        assert_ne!(action_name("A", "x"), action_name("B", "x"));
    }

    #[test]
    fn highest_priority_enabled_context_picks_the_set() {
        let config = config();
        let mut contexts = InputContexts::default();
        assert_eq!(active_context(&config, &contexts, 0), None);
        contexts.push(0, "Gameplay");
        assert_eq!(active_context(&config, &contexts, 0), Some("Gameplay"));
        contexts.push(0, "Menu");
        assert_eq!(active_context(&config, &contexts, 0), Some("Menu"));
        contexts.push(0, "Undeclared");
        assert_eq!(
            active_context(&config, &contexts, 0),
            Some("Menu"),
            "unknown names are ignored"
        );
        assert_eq!(
            active_context(&config, &contexts, 1),
            None,
            "other players have their own stacks"
        );
        contexts.pop(0, "Menu");
        assert_eq!(active_context(&config, &contexts, 0), Some("Gameplay"));
    }

    #[test]
    fn manifest_has_a_set_per_context_with_typed_actions_and_ui() {
        let text = action_manifest(&config()).expect("manifest");
        assert!(text.starts_with("\"Action Manifest\"\n{"));
        assert!(text.contains("\"GAMEPLAY\""));
        assert!(text.contains("\"MENU\""));
        // Gamepad-bound actions only: Jump has a gamepad and a key binding, one Steam action.
        assert_eq!(text.matches("\"gameplay_jump\"").count(), 1);
        assert!(text.contains("\"StickPadGyro\""));
        assert!(text.contains("\"gameplay_move\""));
        assert!(text.contains("\"input_mode\"\t\"joystick_move\""));
        assert!(text.contains("\"AnalogTrigger\""));
        assert!(text.contains("\"gameplay_throttle\""));
        assert!(text.contains("\"menu_confirm\""));
        // Every set carries the UI actions.
        for (suffix, _) in UI_ACTIONS {
            assert!(
                text.contains(&format!("\"gameplay_{suffix}\"")),
                "{suffix} in GAMEPLAY"
            );
            assert!(text.contains(&format!("\"menu_{suffix}\"")), "{suffix} in MENU");
        }
        // Localization carries the original names.
        assert!(text.contains("\"Action_gameplay_jump\"\t\"Jump\""));
        assert!(text.contains("\"Set_MENU\"\t\"Menu\""));
    }

    #[test]
    fn manifest_is_deterministic() {
        assert_eq!(action_manifest(&config()), action_manifest(&config()));
    }

    #[test]
    fn manifest_skips_actions_without_gamepad_bindings() {
        let config = InputConfig::new()
            .action("Look", ActionKind::Axis2D)
            .binding(Binding::new("Look", InputSource::MouseDelta));
        let text = action_manifest(&config).expect("manifest");
        assert!(!text.contains("gameplay_look"));
        assert!(
            text.contains("\"gameplay_ui_up\""),
            "the set still exists for the UI actions"
        );
    }

    #[test]
    fn manifest_rejects_3d_actions_and_name_collisions() {
        let three_d = InputConfig::new()
            .action("Fly", ActionKind::Axis3D)
            .binding(Binding::new("Fly", InputSource::gamepad_stick(Stick::Left)));
        assert_eq!(
            action_manifest(&three_d),
            Err(ManifestError::Unsupported {
                context: "Gameplay".into(),
                action: "Fly".into()
            })
        );

        let clash = InputConfig::new()
            .action("Jump", ActionKind::Button)
            .action("jump", ActionKind::Button)
            .binding(Binding::new(
                "Jump",
                InputSource::gamepad_button(GamepadButton::South),
            ))
            .binding(Binding::new(
                "jump",
                InputSource::gamepad_button(GamepadButton::East),
            ));
        assert!(matches!(
            action_manifest(&clash),
            Err(ManifestError::NameCollision { .. })
        ));

        let sets = InputConfig::new()
            .context("Main Menu", InputContext::new(0))
            .context("Main_Menu", InputContext::new(1));
        assert!(matches!(
            action_manifest(&sets),
            Err(ManifestError::NameCollision { ref name, .. }) if name == "MAIN_MENU"
        ));

        // Non-ASCII folds to `_`, so it collides instead of smuggling a non-ASCII VDF key past
        // the check.
        let unicode = InputConfig::new()
            .context("Menü", InputContext::new(0))
            .context("MEN_", InputContext::new(1));
        assert!(matches!(
            action_manifest(&unicode),
            Err(ManifestError::NameCollision { ref name, .. }) if name == "MEN_"
        ));

        let reserved = InputConfig::new()
            .action("ui_up", ActionKind::Button)
            .binding(Binding::new(
                "ui_up",
                InputSource::gamepad_button(GamepadButton::North),
            ));
        assert!(matches!(
            action_manifest(&reserved),
            Err(ManifestError::NameCollision { .. })
        ));
    }

    #[test]
    fn manifest_from_ron_matches_and_reports_bad_configs() {
        let config = config();
        let ron = config.try_to_ron().expect("config serializes");
        assert_eq!(action_manifest_from_ron(&ron), action_manifest(&config));
        assert!(matches!(
            action_manifest_from_ron("(actions: oops"),
            Err(ManifestError::Config(_))
        ));
        assert_eq!(action_manifest_file_name(480), "game_actions_480.vdf");
    }

    #[test]
    fn manifest_escapes_quotes() {
        assert_eq!(vdf("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
