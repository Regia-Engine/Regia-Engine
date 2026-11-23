// SPDX-License-Identifier: MPL-2.0
//! The cutscene flow around a [`SequencePlayer`] (ADR-2948, book `agents/cutscene.md`).
//!
//! [`start_cutscene`] (or a [`CutsceneCommand::Start`] message) plays a player as a cutscene:
//!
//! - **Input lock** through `regia_input`: for each of [`CutsceneSettings::players`] the enabled
//!   [`InputContexts`] are saved and replaced by the cutscene context alone
//!   ([`CUTSCENE_CONTEXT`] by default, where the host binds the skip action), and toggle latches are
//!   released, so gameplay actions read nothing. The saved contexts come back when the cutscene
//!   hands back, is skipped, or is stopped (also during the pre-roll, and when the player entity is
//!   despawned).
//! - **Letterbox and HUD**: black bars slide in on a `regia_ui` root of their own
//!   ([`LETTERBOX_SORT_ORDER`]) and the UI nodes tagged [`HudLayer`] with a listed name (`"*"`: all)
//!   are hidden (`Display::None`) and given their own display back afterwards. No renderer change.
//! - **Skip**: the skip action ([`SkipSettings::action`]) skips at once, or after it is held for
//!   [`SkipSettings::hold_secs`]; with [`SkipSettings::fade_secs`] the picture fades to black first and
//!   back in after. A skip jumps the player to its last frame ([`SequencePlayer::skip_to_end`]): the
//!   markers in between are delivered in order with `skipped` set, the spawnables of the last frame
//!   are there and the others gone, the last values are applied, and running dialogue lines and
//!   subtitles stop (`dialogue::stop_line`, `SubtitleQueue::clear`).
//! - **Hand-back**: when the player finishes (by playing or by a skip), it is stopped, so the
//!   camera takeover blends back to gameplay (over [`CutsceneSettings::hand_back_blend`] when set),
//!   the spawnables without `keep_on_stop` are despawned and, with `restore_on_stop`, the bound
//!   properties get their prior values. The input and the HUD come back on that frame; the bars
//!   slide out; [`CutsceneEnded`] is sent.
//! - **Multiplayer** ([`NetCutscenes`]): every peer registers the same cutscene name against its own
//!   local player; a start names a tick, each peer starts at that tick and plays locally (the actors
//!   are not replicated), and a peer that is past the tick (a late joiner) skips to the time the
//!   tick says. With the `net` feature, `net::NetCutscenePlugin` carries the start over `regia_net`
//!   (`net::NetCutsceneServer::announce`), late joiners included.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_input::{Actions, InputContexts};
use regia_scene::components::Name;
use regia_ui::node::{Background, BackgroundKind, NodeStyle, UiNode, UiRoot};
use regia_ui::style::{Dimension, Display, FlexDirection, JustifyContent, PositionType, Style};
use regia_ui::{ScaleMode, UiColor};

use super::player::{EndPolicy, PlaybackStatus, SequencePlayer, SequencerSet};

/// The input context a cutscene leaves enabled alone (bind the skip action in it).
pub const CUTSCENE_CONTEXT: &str = "Cutscene";

/// The default skip action name.
pub const SKIP_ACTION: &str = "cutscene_skip";

/// The UI sort order of the letterbox and fade root (above most game UI, below a pause menu at
/// 1000 or more).
pub const LETTERBOX_SORT_ORDER: i32 = 900;

/// Tags a UI node (usually a `UiRoot`) as part of a named HUD layer a cutscene can hide.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct HudLayer(pub String);

/// How a cutscene can be skipped.
#[derive(Clone, Debug, PartialEq)]
pub struct SkipSettings {
    /// `false`: the skip action and [`skip_cutscene`] do nothing.
    pub enabled: bool,
    /// The `regia_input` action that skips.
    pub action: String,
    /// Seconds the action must be held (`0`: a press skips).
    pub hold_secs: f32,
    /// Seconds to fade to black before the jump and back after it (`0`: cut).
    pub fade_secs: f32,
}

impl Default for SkipSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            action: SKIP_ACTION.to_string(),
            hold_secs: 0.0,
            fade_secs: 0.0,
        }
    }
}

/// What a cutscene does around its player.
#[derive(Clone, Debug, PartialEq)]
pub struct CutsceneSettings {
    /// `true` locks gameplay input (see the module docs).
    pub lock_input: bool,
    /// The local players whose input is locked and whose skip action counts.
    pub players: Vec<u8>,
    /// The context left enabled during the cutscene.
    pub input_context: String,
    /// Height of each letterbox bar as a fraction of the screen height, `0..=0.45` (`0`: none).
    pub letterbox: f32,
    /// Seconds the bars take to slide in and out (`0`: at once).
    pub letterbox_secs: f32,
    /// [`HudLayer`] names to hide (`"*"` hides every tagged node).
    pub hide_hud: Vec<String>,
    /// How the cutscene is skipped.
    pub skip: SkipSettings,
    /// Seconds the camera blends back to gameplay at the hand-back; `None` keeps the player's
    /// `takeover.blend_out`.
    pub hand_back_blend: Option<f32>,
    /// `true` stops running dialogue lines and clears the subtitle queue on a skip.
    pub stop_dialogue_on_skip: bool,
    /// Fade of the dialogue lines a skip stops, milliseconds.
    pub dialogue_fade_ms: u32,
}

impl Default for CutsceneSettings {
    fn default() -> Self {
        Self {
            lock_input: true,
            players: vec![0],
            input_context: CUTSCENE_CONTEXT.to_string(),
            letterbox: 0.12,
            letterbox_secs: 0.4,
            hide_hud: vec!["*".to_string()],
            skip: SkipSettings::default(),
            hand_back_blend: None,
            stop_dialogue_on_skip: true,
            dialogue_fade_ms: 100,
        }
    }
}

impl CutsceneSettings {
    /// Checks the ranges: letterbox `0..=0.45`, finite non-negative times, at least one player
    /// when input is locked, non-empty context and action names.
    pub fn validate(&self) -> Result<(), CutsceneError> {
        let bad = |m: String| Err(CutsceneError::BadSettings(m));
        if !(self.letterbox.is_finite() && (0.0..=0.45).contains(&self.letterbox)) {
            return bad(format!("letterbox {} must be in 0..=0.45", self.letterbox));
        }
        for (name, v) in [
            ("letterbox_secs", self.letterbox_secs),
            ("skip.hold_secs", self.skip.hold_secs),
            ("skip.fade_secs", self.skip.fade_secs),
            ("hand_back_blend", self.hand_back_blend.unwrap_or(0.0)),
        ] {
            if !(v.is_finite() && (0.0..=60.0).contains(&v)) {
                return bad(format!("{name} {v} must be in 0..=60"));
            }
        }
        if self.lock_input && self.players.is_empty() {
            return bad("lock_input needs at least one player".into());
        }
        if self.input_context.is_empty() || self.skip.action.is_empty() {
            return bad("input_context and skip.action must not be empty".into());
        }
        Ok(())
    }
}

/// Why a cutscene call was refused (agent codes in [`CutsceneError::code`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CutsceneError {
    /// The entity has no [`SequencePlayer`].
    #[error("entity {0:?} has no SequencePlayer")]
    NoPlayer(Entity),
    /// A cutscene already runs on this player.
    #[error("a cutscene already runs on {0:?}")]
    Running(Entity),
    /// No cutscene runs on this player.
    #[error("no cutscene runs on {0:?}")]
    NotRunning(Entity),
    /// The cutscene cannot be skipped.
    #[error("the cutscene on {0:?} cannot be skipped")]
    SkipDisabled(Entity),
    /// A setting is out of range.
    #[error("bad cutscene settings: {0}")]
    BadSettings(String),
    /// A net cutscene name is not registered on this peer.
    #[error("no cutscene `{0}` is registered")]
    UnknownName(String),
}

impl CutsceneError {
    /// The lowercase code agents see (`cutscene_no_player`, `cutscene_running`,
    /// `cutscene_not_running`, `cutscene_skip_disabled`, `cutscene_bad_settings`,
    /// `cutscene_unknown_name`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoPlayer(_) => "cutscene_no_player",
            Self::Running(_) => "cutscene_running",
            Self::NotRunning(_) => "cutscene_not_running",
            Self::SkipDisabled(_) => "cutscene_skip_disabled",
            Self::BadSettings(_) => "cutscene_bad_settings",
            Self::UnknownName(_) => "cutscene_unknown_name",
        }
    }
}

/// Where a cutscene is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CutscenePhase {
    /// Waiting for the player's spawnables to load.
    #[default]
    Preroll,
    /// The player plays.
    Playing,
    /// A skip fades to black; the jump comes at full black.
    SkipFadeOut,
    /// Handed back: input, HUD and camera are the game's again; the bars slide out and a skip
    /// fade clears.
    HandBack,
}

impl CutscenePhase {
    /// The lowercase name the agent protocol uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Preroll => "preroll",
            Self::Playing => "playing",
            Self::SkipFadeOut => "skip_fade_out",
            Self::HandBack => "hand_back",
        }
    }
}

/// A snapshot of one running cutscene.
#[derive(Clone, Debug, PartialEq)]
pub struct CutsceneState {
    /// The phase.
    pub phase: CutscenePhase,
    /// The settings it runs with.
    pub settings: CutsceneSettings,
    /// `true` once skipped.
    pub skipped: bool,
    /// Seconds the skip action has been held toward [`SkipSettings::hold_secs`].
    pub skip_held: f32,
    /// The skip fade, `0` (clear) to `1` (black).
    pub fade: f32,
    /// How far the letterbox bars are in, `0..=1`.
    pub letterbox: f32,
    /// `true` while gameplay input is locked.
    pub input_locked: bool,
    /// HUD nodes hidden now.
    pub hud_hidden: usize,
    /// The net start tick, for a cutscene started through [`NetCutscenes`].
    pub start_tick: Option<u64>,
}

/// A cutscene ended (handed back, or stopped).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutsceneEnded {
    /// The player entity.
    pub player: Entity,
    /// `true` when it was skipped.
    pub skipped: bool,
    /// `true` when it was stopped (by [`stop_cutscene`], a stopped or despawned player) rather
    /// than played or skipped to its end.
    pub stopped: bool,
}

/// Requests to the cutscene flow (the message form of [`start_cutscene`], [`skip_cutscene`] and
/// [`stop_cutscene`]; refusals are logged).
#[derive(Message, Debug, Clone, PartialEq)]
pub enum CutsceneCommand {
    /// Starts a cutscene.
    Start {
        /// The player entity.
        player: Entity,
        /// Its settings.
        settings: CutsceneSettings,
    },
    /// Skips a running cutscene.
    Skip {
        /// The player entity.
        player: Entity,
    },
    /// Stops a running cutscene.
    Stop {
        /// The player entity.
        player: Entity,
    },
}

/// The UI the cutscene draws.
#[derive(Clone, Copy, Debug)]
struct Overlay {
    root: Entity,
    top: Entity,
    bottom: Entity,
    fade: Entity,
}

/// One running cutscene.
#[derive(Debug)]
struct Run {
    settings: CutsceneSettings,
    phase: CutscenePhase,
    skipped: bool,
    skip_held: f32,
    skip_was_down: bool,
    /// A skip counts only after the skip action was seen released (a button held from before the
    /// cutscene does not skip it).
    skip_armed: bool,
    fade: f32,
    letterbox: f32,
    /// The players this run locked (their contexts are in [`CutsceneLocks`]).
    locked_players: Vec<u8>,
    locked: bool,
    /// The HUD nodes this run hid (their displays are in [`CutsceneLocks`]).
    hidden: Vec<Entity>,
    /// The player's end policy before a skip turned a loop into a hold.
    saved_end: Option<EndPolicy>,
    overlay: Option<Overlay>,
    start_tick: Option<u64>,
    aligned: bool,
    handed_back: bool,
}

/// What overlapping cutscenes share: per local player the input contexts from before the first
/// lock and the context of every cutscene that holds it, per HUD node its display from before the
/// first hide and how many hide it. The first lock saves, the last unlock restores, so two
/// cutscenes that overlap give back exactly what was there before either; an unlock that leaves
/// other cutscenes running gives the player the context of the latest of them back.
#[derive(Resource, Debug, Default)]
pub struct CutsceneLocks {
    input: BTreeMap<u8, InputLock>,
    hud: BTreeMap<Entity, (u32, Option<Display>)>,
}

/// One player's input lock: the contexts saved by the first lock and, in lock order, the context
/// of each cutscene holding it.
#[derive(Debug, Default)]
struct InputLock {
    saved: Vec<String>,
    held: Vec<String>,
}

impl CutsceneLocks {
    /// How many cutscenes lock `player`'s input now.
    #[must_use]
    pub fn input_locks(&self, player: u8) -> u32 {
        self.input
            .get(&player)
            .map_or(0, |lock| u32::try_from(lock.held.len()).unwrap_or(u32::MAX))
    }
}

/// The running cutscenes, by player entity.
#[derive(Resource, Debug, Default)]
pub struct Cutscenes {
    runs: BTreeMap<Entity, Run>,
    started: u64,
    skipped: u64,
}

impl Cutscenes {
    /// The players a cutscene runs on, in entity order.
    pub fn players(&self) -> impl Iterator<Item = Entity> + '_ {
        self.runs.keys().copied()
    }

    /// The state of the cutscene on `player`.
    #[must_use]
    pub fn state(&self, player: Entity) -> Option<CutsceneState> {
        self.runs.get(&player).map(|run| CutsceneState {
            phase: run.phase,
            settings: run.settings.clone(),
            skipped: run.skipped,
            skip_held: run.skip_held,
            fade: run.fade,
            letterbox: run.letterbox,
            input_locked: run.locked,
            hud_hidden: run.hidden.len(),
            start_tick: run.start_tick,
        })
    }

    /// Cutscenes started and skipped since the app started.
    #[must_use]
    pub fn counts(&self) -> (u64, u64) {
        (self.started, self.skipped)
    }
}

/// Starts `player` as a cutscene: locks input, hides the HUD, draws the letterbox, and plays it
/// from the start (a pre-roll first when it has spawnables).
///
/// # Errors
///
/// [`CutsceneError::NoPlayer`], [`CutsceneError::Running`], [`CutsceneError::BadSettings`].
pub fn start_cutscene(
    world: &mut World,
    player: Entity,
    settings: CutsceneSettings,
) -> Result<(), CutsceneError> {
    start_inner(world, player, settings, None, None)
}

fn start_inner(
    world: &mut World,
    player: Entity,
    settings: CutsceneSettings,
    start_tick: Option<u64>,
    net_tick: Option<(u64, f64)>,
) -> Result<(), CutsceneError> {
    settings.validate()?;
    if world.get::<SequencePlayer>(player).is_none() {
        return Err(CutsceneError::NoPlayer(player));
    }
    if world
        .get_resource::<Cutscenes>()
        .is_some_and(|c| c.runs.contains_key(&player))
    {
        return Err(CutsceneError::Running(player));
    }
    let mut run = Run {
        settings,
        phase: CutscenePhase::Preroll,
        skipped: false,
        skip_held: 0.0,
        skip_was_down: false,
        skip_armed: false,
        fade: 0.0,
        letterbox: 0.0,
        locked_players: Vec::new(),
        locked: false,
        hidden: Vec::new(),
        saved_end: None,
        overlay: None,
        start_tick,
        aligned: start_tick.is_none(),
        handed_back: false,
    };
    lock_input(world, &mut run);
    hide_hud(world, &mut run);
    // A skip button already held when the cutscene starts must be let go first.
    let (down, _) = skip_input(world, &run);
    run.skip_was_down = down;
    run.skip_armed = !down;
    if run.settings.letterbox_secs <= 0.0 {
        run.letterbox = 1.0;
    }
    if let Some(mut p) = world.get_mut::<SequencePlayer>(player) {
        p.stop();
        p.play();
        // A late start (the tick is already past the start tick) lands where the tick says; during
        // a pre-roll the skip waits for it.
        if let (Some(start), Some((tick, hz))) = (start_tick, net_tick)
            && tick > start
        {
            p.skip_to(tick.saturating_sub(start) as f64 / hz.max(1.0));
        }
        run.phase = if p.is_prerolling() {
            CutscenePhase::Preroll
        } else {
            CutscenePhase::Playing
        };
    }
    update_overlay(world, &mut run);
    let mut cutscenes = world.get_resource_or_insert_with::<Cutscenes>(Cutscenes::default);
    cutscenes.started += 1;
    cutscenes.runs.insert(player, run);
    Ok(())
}

/// Skips the cutscene on `player` (as the skip action would, with the fade when set). Returns
/// `false` when it is already skipping or handed back.
///
/// # Errors
///
/// [`CutsceneError::NotRunning`], [`CutsceneError::SkipDisabled`].
pub fn skip_cutscene(world: &mut World, player: Entity) -> Result<bool, CutsceneError> {
    let Some(mut cutscenes) = world.remove_resource::<Cutscenes>() else {
        return Err(CutsceneError::NotRunning(player));
    };
    let result = match cutscenes.runs.get_mut(&player) {
        None => Err(CutsceneError::NotRunning(player)),
        Some(run) if !run.settings.skip.enabled => Err(CutsceneError::SkipDisabled(player)),
        Some(run) => {
            let began = begin_skip(world, player, run);
            if began {
                cutscenes.skipped += 1;
            }
            Ok(began)
        }
    };
    world.insert_resource(cutscenes);
    result
}

/// Stops the cutscene on `player` at once: the player stops (camera, spawnables and
/// `restore_on_stop` as on any stop), input and HUD come back, the overlay goes, and
/// [`CutsceneEnded`] is sent with `stopped`. Works during the pre-roll.
///
/// # Errors
///
/// [`CutsceneError::NotRunning`].
pub fn stop_cutscene(world: &mut World, player: Entity) -> Result<(), CutsceneError> {
    let run = world
        .get_resource_mut::<Cutscenes>()
        .and_then(|mut c| c.runs.remove(&player))
        .ok_or(CutsceneError::NotRunning(player))?;
    if let Some(mut p) = world.get_mut::<SequencePlayer>(player) {
        p.stop();
    }
    finish(world, player, run, true);
    Ok(())
}

/// The state of the cutscene on `player`.
#[must_use]
pub fn cutscene_state(world: &World, player: Entity) -> Option<CutsceneState> {
    world.get_resource::<Cutscenes>().and_then(|c| c.state(player))
}

/// Locks the cutscene's players' input: the first lock of a player saves its enabled contexts
/// and leaves only the cutscene context.
fn lock_input(world: &mut World, run: &mut Run) {
    if !run.settings.lock_input || !world.contains_resource::<InputContexts>() {
        return;
    }
    world.init_resource::<CutsceneLocks>();
    world.resource_scope(|world, mut locks: Mut<CutsceneLocks>| {
        let Some(mut contexts) = world.get_resource_mut::<InputContexts>() else {
            return;
        };
        for &p in &run.settings.players {
            if run.locked_players.contains(&p) {
                continue;
            }
            let entry = locks.input.entry(p).or_insert_with(|| InputLock {
                saved: contexts.enabled(p).to_vec(),
                held: Vec::new(),
            });
            entry.held.push(run.settings.input_context.clone());
            contexts.set(p, [run.settings.input_context.clone()]);
            run.locked_players.push(p);
        }
    });
    run.locked = true;
    if let Some(mut actions) = world.get_resource_mut::<Actions>() {
        actions.release_all_toggles();
    }
}

/// Gives the players back: the last unlock of a player restores the contexts saved by the first
/// lock; an earlier one gives the player the context of the latest cutscene still holding it.
fn unlock_input(world: &mut World, run: &mut Run) {
    if !run.locked {
        return;
    }
    run.locked = false;
    let players = std::mem::take(&mut run.locked_players);
    let Some(mut locks) = world.remove_resource::<CutsceneLocks>() else {
        return;
    };
    let mut restore: Vec<(u8, Vec<String>)> = Vec::new();
    for p in players {
        if let Some(entry) = locks.input.get_mut(&p) {
            if let Some(at) = entry.held.iter().rposition(|c| *c == run.settings.input_context) {
                entry.held.remove(at);
            } else {
                entry.held.pop();
            }
            match entry.held.last() {
                Some(context) => restore.push((p, vec![context.clone()])),
                None => {
                    if let Some(lock) = locks.input.remove(&p) {
                        restore.push((p, lock.saved));
                    }
                }
            }
        }
    }
    world.insert_resource(locks);
    if let Some(mut contexts) = world.get_resource_mut::<InputContexts>() {
        for (p, list) in restore {
            if list.is_empty() {
                contexts.remove_player(p);
            } else {
                contexts.set(p, list);
            }
        }
    }
}

/// Hides the HUD layers the settings name; the first hide of a node saves its display.
fn hide_hud(world: &mut World, run: &mut Run) {
    if run.settings.hide_hud.is_empty() {
        return;
    }
    let all = run.settings.hide_hud.iter().any(|n| n == "*");
    let mut query = world.query::<(Entity, &HudLayer, Option<&NodeStyle>)>();
    let mut targets: Vec<(Entity, Option<Display>)> = query
        .iter(world)
        .filter(|(_, layer, _)| all || run.settings.hide_hud.contains(&layer.0))
        .map(|(entity, _, style)| (entity, style.map(|s| s.0.display)))
        .collect();
    targets.sort_unstable_by_key(|(e, _)| e.to_bits());
    let mut locks = world.remove_resource::<CutsceneLocks>().unwrap_or_default();
    for &(entity, display) in &targets {
        let entry = locks.hud.entry(entity).or_insert((0, display));
        entry.0 += 1;
        let Ok(mut node) = world.get_entity_mut(entity) else {
            continue;
        };
        match node.get_mut::<NodeStyle>() {
            Some(mut style) => style.0.display = Display::None,
            None => {
                node.insert(NodeStyle::new(Style {
                    display: Display::None,
                    ..Default::default()
                }));
            }
        }
    }
    world.insert_resource(locks);
    run.hidden = targets.into_iter().map(|(e, _)| e).collect();
}

/// Shows the hidden HUD layers again: the last show of a node gives it its own display back.
fn show_hud(world: &mut World, run: &mut Run) {
    let hidden = std::mem::take(&mut run.hidden);
    if hidden.is_empty() {
        return;
    }
    let Some(mut locks) = world.remove_resource::<CutsceneLocks>() else {
        return;
    };
    for entity in hidden {
        let Some(entry) = locks.hud.get_mut(&entity) else {
            continue;
        };
        entry.0 = entry.0.saturating_sub(1);
        if entry.0 > 0 {
            continue;
        }
        let Some((_, display)) = locks.hud.remove(&entity) else {
            continue;
        };
        let Ok(mut node) = world.get_entity_mut(entity) else {
            continue;
        };
        match display {
            Some(display) => {
                if let Some(mut style) = node.get_mut::<NodeStyle>() {
                    style.0.display = display;
                }
            }
            None => {
                node.remove::<NodeStyle>();
            }
        }
    }
    world.insert_resource(locks);
}

/// Starts a skip: the fade when set, else the jump. `false` when already skipping or done.
fn begin_skip(world: &mut World, player: Entity, run: &mut Run) -> bool {
    if run.skipped || matches!(run.phase, CutscenePhase::SkipFadeOut | CutscenePhase::HandBack) {
        return false;
    }
    if run.settings.skip.fade_secs > 0.0 {
        run.phase = CutscenePhase::SkipFadeOut;
    } else {
        jump(world, player, run);
    }
    true
}

/// The jump of a skip: the player to its last frame, dialogue and subtitles stopped.
fn jump(world: &mut World, player: Entity, run: &mut Run) {
    run.skipped = true;
    if let Some(mut p) = world.get_mut::<SequencePlayer>(player) {
        // A looping player skips to its end too and finishes there.
        if p.end == EndPolicy::Loop {
            run.saved_end = Some(p.end);
            p.end = EndPolicy::Hold;
        }
        p.skip_to_end();
    }
    // The run loop hands back once the player reports finished (after the pre-roll, if any).
    if run.phase != CutscenePhase::Preroll {
        run.phase = CutscenePhase::Playing;
    }
    if run.settings.stop_dialogue_on_skip {
        if let Err(error) = crate::dialogue::stop_line(world, None, run.settings.dialogue_fade_ms) {
            tracing::debug!(target: "regia_framework", "cutscene skip: dialogue: {error}");
        }
        if let Some(mut queue) = world.get_resource_mut::<regia_ui::SubtitleQueue>() {
            queue.clear();
        }
    }
}

/// Hands the game back: stops the player (camera blend out, spawnables, restore), unlocks input
/// and shows the HUD.
fn hand_back(world: &mut World, player: Entity, run: &mut Run) {
    if run.handed_back {
        return;
    }
    run.handed_back = true;
    if let Some(mut p) = world.get_mut::<SequencePlayer>(player) {
        if let Some(blend) = run.settings.hand_back_blend {
            p.takeover.blend_out = blend;
        }
        p.stop();
    }
    unlock_input(world, run);
    show_hud(world, run);
    run.phase = CutscenePhase::HandBack;
}

/// Ends a run: whatever is still the cutscene's goes back, the overlay goes, the message goes out.
fn finish(world: &mut World, player: Entity, mut run: Run, stopped: bool) {
    unlock_input(world, &mut run);
    show_hud(world, &mut run);
    if let Some(end) = run.saved_end.take()
        && let Some(mut p) = world.get_mut::<SequencePlayer>(player)
    {
        p.end = end;
    }
    if let Some(overlay) = run.overlay.take() {
        regia_scene::hierarchy::despawn_tree(world, overlay.root);
    }
    world.write_message(CutsceneEnded {
        player,
        skipped: run.skipped,
        stopped,
    });
}

/// `true` when the skip action of the run's players is down; `pressed` when it went down this
/// frame.
fn skip_input(world: &World, run: &Run) -> (bool, bool) {
    let Some(actions) = world.get_resource::<Actions>() else {
        return (false, false);
    };
    let mut down = false;
    let mut pressed = false;
    for &p in &run.settings.players {
        if let Some(state) = actions
            .players
            .get(&p)
            .and_then(|pa| pa.states.get(&run.settings.skip.action))
        {
            down |= state.actuated();
            pressed |= state.just_pressed();
        }
    }
    (down, pressed)
}

/// Creates or updates the bars and the fade overlay; removes them when nothing shows.
fn update_overlay(world: &mut World, run: &mut Run) {
    let bar = run.settings.letterbox * run.letterbox;
    let shows = bar > 0.0 || run.fade > 0.0;
    if !shows {
        if let Some(overlay) = run.overlay.take() {
            regia_scene::hierarchy::despawn_tree(world, overlay.root);
        }
        return;
    }
    let overlay = match run.overlay {
        Some(overlay) if world.get_entity(overlay.root).is_ok() => overlay,
        _ => {
            let overlay = spawn_overlay(world);
            run.overlay = Some(overlay);
            overlay
        }
    };
    let percent = Dimension::Percent(bar * 100.0);
    for entity in [overlay.top, overlay.bottom] {
        if let Some(mut style) = world.get_mut::<NodeStyle>(entity) {
            style.0.size = [Dimension::Percent(100.0), percent];
        }
    }
    if let Some(mut background) = world.get_mut::<Background>(overlay.fade) {
        background.kind = BackgroundKind::Color(UiColor([0.0, 0.0, 0.0, run.fade.clamp(0.0, 1.0)]));
    }
}

/// The letterbox root: a full-screen column with a bar at the top and the bottom, and a fade
/// layer over the whole screen.
fn spawn_overlay(world: &mut World) -> Overlay {
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                sort_order: LETTERBOX_SORT_ORDER,
                ..Default::default()
            },
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                justify_content: JustifyContent::SpaceBetween,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
            Name::new("cutscene_letterbox"),
        ))
        .id();
    let bar = |world: &mut World| {
        world
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Percent(0.0)],
                    ..Default::default()
                }),
                Background {
                    kind: BackgroundKind::Color(UiColor::BLACK),
                },
            ))
            .id()
    };
    let top = bar(world);
    let bottom = bar(world);
    let fade = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor([0.0, 0.0, 0.0, 0.0])),
            },
        ))
        .id();
    let mut parent = world.entity_mut(root);
    parent.add_child(top);
    parent.add_child(bottom);
    parent.add_child(fade);
    Overlay {
        root,
        top,
        bottom,
        fade,
    }
}

/// Moves `value` toward `target` by `dt / secs` (at once when `secs` is zero).
fn approach(value: f32, target: f32, dt: f32, secs: f32) -> f32 {
    if secs <= 0.0 {
        return target;
    }
    let step = dt / secs;
    if value < target {
        (value + step).min(target)
    } else {
        (value - step).max(target)
    }
}

/// Runs every cutscene one frame: commands, net starts, skip input, fades, hand-back.
pub(super) fn run_cutscenes(world: &mut World) {
    let commands: Vec<CutsceneCommand> = world
        .get_resource_mut::<Messages<CutsceneCommand>>()
        .map(|mut m| m.drain().collect())
        .unwrap_or_default();
    for command in commands {
        let result = match command {
            CutsceneCommand::Start { player, settings } => start_cutscene(world, player, settings),
            CutsceneCommand::Skip { player } => skip_cutscene(world, player).map(|_| ()),
            CutsceneCommand::Stop { player } => stop_cutscene(world, player),
        };
        if let Err(error) = result {
            tracing::warn!(target: "regia_framework", code = error.code(), "cutscene command: {error}");
        }
    }
    run_net_cutscenes(world);
    let Some(mut cutscenes) = world.remove_resource::<Cutscenes>() else {
        return;
    };
    if cutscenes.runs.is_empty() {
        world.insert_resource(cutscenes);
        return;
    }
    let _span = tracing::info_span!(target: "regia_framework", "sequencer_cutscenes").entered();
    let dt = world.get_resource::<Time>().map_or(0.0, |t| t.delta_secs());
    let net_time = world
        .get_resource::<NetCutscenes>()
        .and_then(|n| n.tick.map(|t| (t, n.tick_hz)));
    let mut ended: Vec<(Entity, bool)> = Vec::new();
    let mut skips = 0u64;
    let players: Vec<Entity> = cutscenes.runs.keys().copied().collect();
    for player in players {
        let Some(run) = cutscenes.runs.get_mut(&player) else {
            continue;
        };
        let Some((status, prerolling, time)) = world
            .get::<SequencePlayer>(player)
            .map(|p| (p.status(), p.is_prerolling(), p.time()))
        else {
            // The player entity is gone: give everything back.
            ended.push((player, true));
            continue;
        };
        if !run.handed_back && status == PlaybackStatus::Stopped {
            // Stopped from outside (an agent, the game): that ends the cutscene.
            ended.push((player, true));
            continue;
        }
        if run.phase == CutscenePhase::Preroll && !prerolling {
            run.phase = CutscenePhase::Playing;
        }
        // A net start lands where the start tick says once the pre-roll is over.
        if !run.aligned
            && !prerolling
            && let (Some(start), Some((tick, hz))) = (run.start_tick, net_time)
        {
            run.aligned = true;
            let target = tick.saturating_sub(start) as f64 / hz.max(1.0);
            if target > time
                && let Some(mut p) = world.get_mut::<SequencePlayer>(player)
            {
                p.skip_to(target);
            }
        }
        // Skip input.
        if run.settings.skip.enabled
            && matches!(run.phase, CutscenePhase::Preroll | CutscenePhase::Playing)
            && !run.skipped
        {
            let (down, pressed) = skip_input(world, run);
            if !down {
                run.skip_armed = true;
            }
            let trigger = if !run.skip_armed {
                false
            } else if run.settings.skip.hold_secs > 0.0 {
                run.skip_held = if down { run.skip_held + dt } else { 0.0 };
                run.skip_held >= run.settings.skip.hold_secs
            } else {
                pressed || (down && !run.skip_was_down)
            };
            run.skip_was_down = down;
            if trigger && begin_skip(world, player, run) {
                skips += 1;
            }
        }
        // The skip fade.
        match run.phase {
            CutscenePhase::SkipFadeOut => {
                run.fade = approach(run.fade, 1.0, dt, run.settings.skip.fade_secs);
                // At black, and only once the pre-roll is over: the picture stays black until the
                // jump can land.
                if run.fade >= 1.0 && !prerolling {
                    jump(world, player, run);
                }
            }
            _ if run.fade > 0.0 => {
                run.fade = approach(run.fade, 0.0, dt, run.settings.skip.fade_secs);
            }
            _ => {}
        }
        // Hand back at the end (played through, or skipped to it).
        // Only once the last frame was applied (and its markers and spawnables went out): a skip
        // between frames finishes the player before this frame's apply.
        let finished = world
            .get::<SequencePlayer>(player)
            .is_some_and(|p| p.status() == PlaybackStatus::Finished && p.frame_applied());
        if finished && !run.handed_back && run.phase != CutscenePhase::SkipFadeOut {
            hand_back(world, player, run);
        }
        // The bars.
        let target = if run.handed_back { 0.0 } else { 1.0 };
        run.letterbox = approach(run.letterbox, target, dt, run.settings.letterbox_secs);
        update_overlay(world, run);
        if run.handed_back && run.letterbox <= 0.0 && run.fade <= 0.0 {
            ended.push((player, false));
        }
    }
    cutscenes.skipped += skips;
    for (player, stopped) in ended {
        if let Some(run) = cutscenes.runs.remove(&player) {
            if stopped && let Some(mut p) = world.get_mut::<SequencePlayer>(player) {
                p.stop();
            }
            finish(world, player, run, stopped);
        }
    }
    world.insert_resource(cutscenes);
}

/// Cutscenes every peer of a multiplayer session plays together (see the module docs).
#[derive(Resource, Debug)]
pub struct NetCutscenes {
    /// Simulation ticks per second (what turns a tick difference into sequence seconds).
    pub tick_hz: f64,
    named: BTreeMap<String, (Entity, CutsceneSettings)>,
    scheduled: BTreeMap<String, u64>,
    running: BTreeMap<String, (Entity, u64)>,
    /// Every (name, start tick) started here: a start is played once, however often it is
    /// announced.
    started: std::collections::BTreeSet<(String, u64)>,
    tick: Option<u64>,
}

impl Default for NetCutscenes {
    fn default() -> Self {
        Self {
            tick_hz: 60.0,
            named: BTreeMap::new(),
            scheduled: BTreeMap::new(),
            running: BTreeMap::new(),
            started: std::collections::BTreeSet::new(),
            tick: None,
        }
    }
}

impl NetCutscenes {
    /// Registers `name` as the cutscene this peer plays on its own `player` entity.
    pub fn register(&mut self, name: impl Into<String>, player: Entity, settings: CutsceneSettings) {
        self.named.insert(name.into(), (player, settings));
    }

    /// Schedules `name` to start at `start_tick` (a start already running, or this exact start
    /// already played, is left alone; a second schedule of a pending one keeps the earlier tick).
    pub fn schedule(&mut self, name: impl Into<String>, start_tick: u64) {
        let name = name.into();
        if self.running.contains_key(&name) || self.started.contains(&(name.clone(), start_tick)) {
            return;
        }
        let entry = self.scheduled.entry(name).or_insert(start_tick);
        *entry = (*entry).min(start_tick);
    }

    /// Sets the current simulation tick (the `net` glue writes the session tick each frame; a
    /// host without `regia_net` writes its own).
    pub fn set_tick(&mut self, tick: u64) {
        self.tick = Some(tick);
    }

    /// The current tick, when known.
    #[must_use]
    pub fn tick(&self) -> Option<u64> {
        self.tick
    }

    /// The cutscenes waiting for their tick, with it.
    pub fn scheduled(&self) -> impl Iterator<Item = (&str, u64)> {
        self.scheduled.iter().map(|(n, t)| (n.as_str(), *t))
    }

    /// The cutscenes started through this resource, with their player and start tick (what a
    /// server sends a late joiner).
    pub fn running(&self) -> impl Iterator<Item = (&str, Entity, u64)> {
        self.running.iter().map(|(n, (e, t))| (n.as_str(), *e, *t))
    }
}

/// Starts each scheduled cutscene whose tick came, and forgets the running ones that ended.
fn run_net_cutscenes(world: &mut World) {
    let Some(mut net) = world.remove_resource::<NetCutscenes>() else {
        return;
    };
    let live: Vec<Entity> = world
        .get_resource::<Cutscenes>()
        .map(|c| c.runs.keys().copied().collect())
        .unwrap_or_default();
    net.running.retain(|_, (player, _)| live.contains(player));
    if let Some(tick) = net.tick {
        let due: Vec<(String, u64)> = net
            .scheduled
            .iter()
            .filter(|(_, start)| **start <= tick)
            .map(|(n, t)| (n.clone(), *t))
            .collect();
        for (name, start) in due {
            net.scheduled.remove(&name);
            net.started.insert((name.clone(), start));
            let Some((player, settings)) = net.named.get(&name).cloned() else {
                tracing::warn!(target: "regia_framework", code = "cutscene_unknown_name", %name, "net cutscene not registered here");
                continue;
            };
            match start_inner(world, player, settings, Some(start), Some((tick, net.tick_hz))) {
                Ok(()) => {
                    net.running.insert(name, (player, start));
                }
                Err(error) => {
                    tracing::warn!(target: "regia_framework", code = error.code(), %name, "net cutscene: {error}");
                }
            }
        }
    }
    world.insert_resource(net);
}

/// Installs the cutscene flow (idempotent; [`super::SequencerPlugin`] calls it).
pub(super) fn install(app: &mut App) {
    if app.world().contains_resource::<Cutscenes>() {
        return;
    }
    app.init_resource::<Cutscenes>()
        .init_resource::<NetCutscenes>()
        .init_resource::<CutsceneLocks>()
        .add_message::<CutsceneEnded>()
        .add_message::<CutsceneCommand>()
        .add_systems(PostUpdate, run_cutscenes.before(SequencerSet::Advance));
}

#[cfg(feature = "net")]
pub mod net;
