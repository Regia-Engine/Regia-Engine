// SPDX-License-Identifier: MPL-2.0
//! The mixer dock's effect chains, sends and snapshots (parity row P-227, `audio-buses` v1,
//! ADR-8105 (g), ADR-8354): per strip an effect chain (add, remove, reorder, bypass) and a send knob
//! per send bus; below the strips the named snapshots (save, apply with their blend time, release,
//! delete). Every chain, send and snapshot change is one [`LayoutEdit`] on the undo stack; apply and
//! release audition a snapshot on the pumped world's mixer and are not undo steps. Save writes the
//! whole layout to `config/audio.ron`.
//!
//! Each send row has a Pre toggle: a pre-fader send taps the bus before its fader and mute
//! ([`SendTap`], ADR-8590). The tap is structure, not a level: toggling it is its own undo step and
//! snapshots never store or blend it.
//!
//! Agent surface: `editor.audio_mixer_chain`, `editor.audio_mixer_send`, `editor.audio_mixer_snapshot`
//! and `editor.audio_mixer_save` (`book/agents/audio-mixer.md`).

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_audio::{AudioSnapshot, BUS_EFFECT_KINDS, BusEffect, BusSend, SendTap, SnapshotSend};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::{AudioMixerPanel, FADER_MAX_DB, HINT, MixerBus, err, state_json, take_panel};
use crate::command::{EditorCommand, UndoStack};
use crate::data_panels::{PanelError, opt_bool, opt_f32};
use crate::viewport_agent::Handler;

/// The longest effect chain a strip takes.
pub const MAX_CHAIN: usize = 16;

/// The most snapshots a layout holds.
pub const MAX_SNAPSHOTS: usize = 64;

/// The lowest send level a knob sets, dB (silence to the mixer).
pub const SEND_MIN_DB: f32 = regia_audio::SILENT_SEND_DB;

/// The longest snapshot blend, seconds.
pub const MAX_BLEND_SECS: f32 = 60.0;

/// The chain, send and snapshot part of the mixer model: what a [`LayoutEdit`] swaps.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutState {
    /// Every bus (fader, mute, solo, chain, sends).
    pub buses: Vec<MixerBus>,
    /// The named snapshots.
    pub snapshots: Vec<AudioSnapshot>,
}

/// One reversible chain, send or snapshot change (one undo step).
pub struct LayoutEdit {
    label: String,
    before: LayoutState,
    after: LayoutState,
}

impl LayoutEdit {
    fn set(world: &mut World, state: &LayoutState) {
        let mut panel = take_panel(world);
        panel.buses = state.buses.clone();
        panel.snapshots = state.snapshots.clone();
        panel.generation += 1;
        panel.layout_generation += 1;
        panel.edits += 1;
        world.insert_resource(panel);
    }

    /// Whether the edit changes only levels a slider scrubs (send levels, snapshot blend
    /// times): the two states are equal once those are cleared.
    fn level_only(&self) -> bool {
        fn levels_cleared(state: &LayoutState) -> LayoutState {
            let mut out = state.clone();
            for bus in &mut out.buses {
                for send in &mut bus.sends {
                    send.volume_db = 0.0;
                }
            }
            for snapshot in &mut out.snapshots {
                snapshot.blend_secs = 0.0;
            }
            out
        }
        levels_cleared(&self.before) == levels_cleared(&self.after)
    }
}

impl EditorCommand for LayoutEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<LayoutEdit>() else {
            return false;
        };
        // A send-knob or blend-time scrub: consecutive level-only moves of the same control are
        // one step (the label names the route or the snapshot).
        if self.label == next.label && self.level_only() && next.level_only() && self.after == next.before {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        format!("Mixer: {}", self.label)
    }
}

/// Runs `change` on a copy of the model; when it succeeds and changed something, commits the
/// result as one undo step. `Ok(false)` when nothing changed.
fn commit(
    world: &mut World,
    label: String,
    change: impl FnOnce(&mut LayoutState) -> Result<(), PanelError>,
) -> Result<bool, PanelError> {
    let panel = take_panel(world);
    let before = panel.layout_state();
    world.insert_resource(panel);
    let mut after = before.clone();
    change(&mut after)?;
    if after == before {
        return Ok(false);
    }
    let mut command = LayoutEdit { label, before, after };
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    Ok(true)
}

fn bus_mut<'a>(state: &'a mut LayoutState, bus: &str) -> Result<&'a mut MixerBus, PanelError> {
    let names = state.buses.iter().map(|b| b.name.clone()).collect::<Vec<_>>();
    state.buses.iter_mut().find(|b| b.name == bus).ok_or_else(|| {
        err(
            "unknown_bus",
            format!("no bus named `{bus}` (buses: {})", names.join(", ")),
        )
    })
}

fn bad_index(index: usize, len: usize) -> PanelError {
    err(
        "bad_index",
        format!("effect index {index} is past the chain (it has {len} effects)"),
    )
}

/// One change to a strip's effect chain.
#[derive(Clone, Debug, PartialEq)]
pub enum ChainOp {
    /// Inserts a new effect of the kind ([`BUS_EFFECT_KINDS`]) at the index (`None` = the end).
    Add {
        /// The effect kind.
        kind: String,
        /// Where it goes.
        index: Option<usize>,
    },
    /// Removes the effect at the index.
    Remove {
        /// The effect.
        index: usize,
    },
    /// Moves the effect at `from` to `to` (both indices of the chain as it is).
    Move {
        /// The effect.
        from: usize,
        /// Its new place.
        to: usize,
    },
    /// Bypasses (`true`) or re-activates the effect at the index.
    Bypass {
        /// The effect.
        index: usize,
        /// Bypassed.
        bypass: bool,
    },
}

/// Applies `op` to `bus`'s chain as one undo step. `Ok(false)` when nothing changed.
///
/// # Errors
///
/// `audio_mixer_unknown_bus`, `audio_mixer_unknown_effect` (a kind not in [`BUS_EFFECT_KINDS`]),
/// `audio_mixer_bad_index`, `audio_mixer_chain_full` (more than [`MAX_CHAIN`] effects).
pub fn edit_chain(world: &mut World, bus: &str, op: &ChainOp) -> Result<bool, PanelError> {
    let label = match op {
        ChainOp::Add { kind, .. } => format!("add {kind} on {bus}"),
        ChainOp::Remove { .. } => format!("remove an effect on {bus}"),
        ChainOp::Move { .. } => format!("reorder the chain of {bus}"),
        ChainOp::Bypass { bypass: true, .. } => format!("bypass an effect on {bus}"),
        ChainOp::Bypass { bypass: false, .. } => format!("activate an effect on {bus}"),
    };
    commit(world, label, |state| {
        let strip = bus_mut(state, bus)?;
        let chain = &mut strip.effects;
        let len = chain.len();
        match op {
            ChainOp::Add { kind, index } => {
                let effect = BusEffect::of_kind(kind).ok_or_else(|| {
                    err(
                        "unknown_effect",
                        format!("no effect kind `{kind}` (kinds: {})", BUS_EFFECT_KINDS.join(", ")),
                    )
                })?;
                if len >= MAX_CHAIN {
                    return Err(err(
                        "chain_full",
                        format!("a chain holds at most {MAX_CHAIN} effects"),
                    ));
                }
                let at = index.unwrap_or(len);
                if at > len {
                    return Err(bad_index(at, len));
                }
                chain.insert(at, effect);
            }
            ChainOp::Remove { index } => {
                if *index >= len {
                    return Err(bad_index(*index, len));
                }
                chain.remove(*index);
            }
            ChainOp::Move { from, to } => {
                if *from >= len || *to >= len {
                    return Err(bad_index((*from).max(*to), len));
                }
                let effect = chain.remove(*from);
                chain.insert(*to, effect);
            }
            ChainOp::Bypass { index, bypass } => {
                let Some(slot) = chain.get_mut(*index) else {
                    return Err(bad_index(*index, len));
                };
                *slot = slot.clone().with_bypass(*bypass);
            }
        }
        Ok(())
    })
}

/// Sets the level of `bus`'s send into the send bus `send` (adding the route when it has none),
/// or removes the route (`volume_db: None`), as one undo step. `Ok(false)` when nothing changed.
///
/// # Errors
///
/// `audio_mixer_unknown_bus`, `audio_mixer_unknown_send` (not a send bus of the layout),
/// `audio_mixer_bad_value` (a level outside [`SEND_MIN_DB`]`..=`[`FADER_MAX_DB`]).
pub fn edit_send(
    world: &mut World,
    bus: &str,
    send: &str,
    volume_db: Option<f32>,
) -> Result<bool, PanelError> {
    check_send(world, send, volume_db)?;
    let label = match volume_db {
        Some(_) => format!("send {bus} → {send}"),
        None => format!("remove the send {bus} → {send}"),
    };
    commit(world, label, |state| {
        let strip = bus_mut(state, bus)?;
        match volume_db {
            Some(db) => match strip.sends.iter_mut().find(|s| s.bus == send) {
                Some(route) => route.volume_db = db,
                None => strip.sends.push(BusSend::new(send, db)),
            },
            None => strip.sends.retain(|s| s.bus != send),
        }
        // A snapshot may not keep a level for a route that no longer exists.
        let routes: Vec<(String, String)> = state
            .buses
            .iter()
            .flat_map(|b| b.sends.iter().map(|s| (b.name.clone(), s.bus.clone())))
            .collect();
        for snapshot in &mut state.snapshots {
            snapshot
                .send_volumes
                .retain(|s| routes.iter().any(|(b, x)| *b == s.bus && *x == s.send));
        }
        Ok(())
    })
}

/// Refuses a send bus the layout does not have and a level outside the knob's range.
fn check_send(world: &mut World, send: &str, volume_db: Option<f32>) -> Result<(), PanelError> {
    let panel = take_panel(world);
    let send_buses: Vec<String> = panel.layout.send_buses.iter().map(|s| s.name.clone()).collect();
    world.insert_resource(panel);
    if !send_buses.iter().any(|s| s == send) {
        return Err(err(
            "unknown_send",
            format!(
                "no send bus named `{send}` (send buses: {})",
                send_buses.join(", ")
            ),
        ));
    }
    if let Some(db) = volume_db
        && (!db.is_finite() || !(SEND_MIN_DB..=FADER_MAX_DB).contains(&db))
    {
        return Err(err(
            "bad_value",
            format!("a send level must be within {SEND_MIN_DB}..={FADER_MAX_DB} dB, got {db}"),
        ));
    }
    Ok(())
}

/// Sets where `bus`'s send into `send` taps the bus ([`SendTap::PreFader`]: before its fader and
/// mute; [`SendTap::PostFader`]: after them), and its level when `volume_db` is given, as one undo
/// step. A missing route is added at `volume_db` (else silent, [`SEND_MIN_DB`]). The tap is
/// structure: the pumped mixer rebuilds the bus's tracks (ADR-8590). `Ok(false)` when nothing
/// changed.
///
/// # Errors
///
/// `audio_mixer_unknown_bus`, `audio_mixer_unknown_send`, `audio_mixer_bad_value`, as
/// [`edit_send`].
pub fn edit_send_tap(
    world: &mut World,
    bus: &str,
    send: &str,
    tap: SendTap,
    volume_db: Option<f32>,
) -> Result<bool, PanelError> {
    check_send(world, send, volume_db)?;
    let label = format!("{} send {bus} → {send}", tap_label(tap));
    commit(world, label, |state| {
        let strip = bus_mut(state, bus)?;
        match strip.sends.iter_mut().find(|s| s.bus == send) {
            Some(route) => {
                route.tap = tap;
                if let Some(db) = volume_db {
                    route.volume_db = db;
                }
            }
            None => strip
                .sends
                .push(BusSend::new(send, volume_db.unwrap_or(SEND_MIN_DB)).with_tap(tap)),
        }
        Ok(())
    })
}

/// `pre-fader` / `post-fader`.
fn tap_label(tap: SendTap) -> &'static str {
    match tap {
        SendTap::PreFader => "pre-fader",
        SendTap::PostFader => "post-fader",
    }
}

/// The agent spelling of a tap: `pre_fader` / `post_fader`.
pub(super) fn tap_name(tap: SendTap) -> &'static str {
    match tap {
        SendTap::PreFader => "pre_fader",
        SendTap::PostFader => "post_fader",
    }
}

/// One snapshot change.
#[derive(Clone, Debug, PartialEq)]
pub enum SnapshotOp {
    /// Captures every bus fader, mute and send level into the named snapshot (replacing one of
    /// that name), with the blend time (`None` keeps the replaced snapshot's, else 0.5 s).
    Save {
        /// Snapshot name.
        name: String,
        /// Blend time, seconds.
        blend_secs: Option<f32>,
    },
    /// Sets a snapshot's blend time.
    Blend {
        /// Snapshot name.
        name: String,
        /// Blend time, seconds.
        blend_secs: f32,
    },
    /// Deletes a snapshot.
    Delete {
        /// Snapshot name.
        name: String,
    },
    /// Pushes the snapshot on the pumped world's mixer, blending over its blend time (at the next
    /// editor frame; not an undo step).
    Apply {
        /// Snapshot name.
        name: String,
    },
    /// Pops the snapshot on top of the pumped world's mixer, blending back over its blend time.
    Release,
}

/// A snapshot audition queued for the next pump.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Audition {
    Apply(AudioSnapshot),
    Release(f32),
}

fn check_blend(secs: f32) -> Result<f32, PanelError> {
    if secs.is_finite() && (0.0..=MAX_BLEND_SECS).contains(&secs) {
        Ok(secs)
    } else {
        Err(err(
            "bad_value",
            format!("`blend_secs` must be within 0..={MAX_BLEND_SECS}, got {secs}"),
        ))
    }
}

fn unknown_snapshot(name: &str, state: &[AudioSnapshot]) -> PanelError {
    let names: Vec<&str> = state.iter().map(|s| s.name.as_str()).collect();
    err(
        "unknown_snapshot",
        format!("no snapshot named `{name}` (snapshots: {})", names.join(", ")),
    )
}

/// The snapshot `name` taken from the model now: every bus fader and mute, every send level.
fn capture(state: &LayoutState, name: &str, blend_secs: f32) -> AudioSnapshot {
    AudioSnapshot {
        name: name.to_owned(),
        bus_volumes: state
            .buses
            .iter()
            .map(|b| (b.name.clone(), b.volume_db))
            .collect(),
        filter_cutoffs: Default::default(),
        bus_mutes: state.buses.iter().map(|b| (b.name.clone(), b.muted)).collect(),
        send_volumes: state
            .buses
            .iter()
            .flat_map(|b| {
                b.sends.iter().map(|s| SnapshotSend {
                    bus: b.name.clone(),
                    send: s.bus.clone(),
                    volume_db: s.volume_db,
                })
            })
            .collect(),
        blend_secs,
    }
}

/// Applies `op`. `Ok(false)` when nothing changed (no undo entry; apply and release never add one).
///
/// # Errors
///
/// `audio_mixer_bad_name` (empty, longer than 64 bytes, or a control character),
/// `audio_mixer_unknown_snapshot`, `audio_mixer_bad_value` (a blend time outside 0..=60 s),
/// `audio_mixer_too_many_snapshots`, `audio_mixer_no_snapshot` (release with nothing applied).
pub fn edit_snapshot(world: &mut World, op: &SnapshotOp) -> Result<bool, PanelError> {
    match op {
        SnapshotOp::Save { name, blend_secs } => {
            let name = name.trim();
            if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
                return Err(err(
                    "bad_name",
                    "a snapshot name is 1 to 64 bytes with no control characters",
                ));
            }
            let blend = blend_secs.map(check_blend).transpose()?;
            commit(world, format!("save snapshot {name}"), |state| {
                let keep = state.snapshots.iter().position(|s| s.name == name);
                let secs = blend
                    .or_else(|| keep.map(|i| state.snapshots[i].blend_secs))
                    .unwrap_or(0.5);
                let snapshot = capture(state, name, secs);
                match keep {
                    Some(i) => state.snapshots[i] = snapshot,
                    None if state.snapshots.len() >= MAX_SNAPSHOTS => {
                        return Err(err(
                            "too_many_snapshots",
                            format!("a layout holds at most {MAX_SNAPSHOTS} snapshots"),
                        ));
                    }
                    None => state.snapshots.push(snapshot),
                }
                Ok(())
            })
        }
        SnapshotOp::Blend { name, blend_secs } => {
            let secs = check_blend(*blend_secs)?;
            commit(world, format!("blend time of {name}"), |state| {
                let all = state.snapshots.clone();
                let snapshot = state
                    .snapshots
                    .iter_mut()
                    .find(|s| s.name == *name)
                    .ok_or_else(|| unknown_snapshot(name, &all))?;
                snapshot.blend_secs = secs;
                Ok(())
            })
        }
        SnapshotOp::Delete { name } => commit(world, format!("delete snapshot {name}"), |state| {
            let before = state.snapshots.len();
            state.snapshots.retain(|s| s.name != *name);
            if state.snapshots.len() == before {
                return Err(unknown_snapshot(name, &state.snapshots));
            }
            Ok(())
        }),
        SnapshotOp::Apply { name } => {
            let mut panel = take_panel(world);
            let result = match panel.snapshots.iter().find(|s| s.name == *name) {
                Some(snapshot) => {
                    panel.auditions.push(Audition::Apply(snapshot.clone()));
                    panel.applied.push(name.clone());
                    Ok(true)
                }
                None => Err(unknown_snapshot(name, &panel.snapshots)),
            };
            world.insert_resource(panel);
            result
        }
        SnapshotOp::Release => {
            let mut panel = take_panel(world);
            let result = match panel.applied.pop() {
                Some(name) => {
                    let secs = panel
                        .snapshots
                        .iter()
                        .find(|s| s.name == name)
                        .map_or(0.0, |s| s.blend_secs);
                    panel.auditions.push(Audition::Release(secs));
                    Ok(true)
                }
                None => Err(err("no_snapshot", "no snapshot is applied")),
            };
            world.insert_resource(panel);
            result
        }
    }
}

/// Writes the mixer's layout (chains, sends, faders, mutes, snapshots) to the project's
/// `config/audio.ron` at the current `audio-buses` version. Returns the path written.
///
/// # Errors
///
/// `audio_mixer_no_project`, `audio_mixer_bad_layout` (the layout fails validation),
/// `audio_mixer_io_failed`.
pub fn save_layout(world: &mut World) -> Result<std::path::PathBuf, PanelError> {
    let Some(root) = crate::mesh_source::project_root(world) else {
        return Err(err("no_project", "no project is open"));
    };
    let panel = take_panel(world);
    let layout = panel.layout_now();
    world.insert_resource(panel);
    layout.validate().map_err(|e| err("bad_layout", e))?;
    let text = layout.to_ron().map_err(|e| err("bad_layout", e))?;
    let path = root.join(crate::creators::AUDIO_CONFIG_PATH);
    crate::data_panels::write_atomic(&path, &text, "audio_mixer")?;
    let mut panel = take_panel(world);
    panel.saved_edits = panel.edits;
    world.insert_resource(panel);
    Ok(path)
}

// --------------------------------------------------------------------------------------- drawing

/// What one strip's chain editor asked for this frame.
pub(super) enum StripRequest {
    Chain(ChainOp),
    Send(String, Option<f32>),
    Tap(String, SendTap),
}

/// The chain editor and send knobs of one strip (shown under the strip when it is expanded).
pub(super) fn strip_chain_ui(
    ui: &mut egui::Ui,
    tokens: &crate::theme::ThemeTokens,
    panel: &AudioMixerPanel,
    bus: &MixerBus,
    add_kind: &mut usize,
) -> Vec<StripRequest> {
    let mut out = Vec::new();
    ui.label(egui::RichText::new(format!("{} effect chain", bus.name)).strong());
    if bus.effects.is_empty() {
        ui.label(egui::RichText::new("No effects").color(tokens.muted));
    }
    let last = bus.effects.len().saturating_sub(1);
    for (i, effect) in bus.effects.iter().enumerate() {
        ui.horizontal(|ui| {
            let name = effect_label(effect);
            let text = egui::RichText::new(format!("{}. {name}", i + 1));
            ui.label(if effect.is_bypassed() {
                text.color(tokens.muted).strikethrough()
            } else {
                text
            });
            let mut bypass = effect.is_bypassed();
            if ui
                .toggle_value(&mut bypass, "Bypass")
                .on_hover_text(format!("Bypass {name} on {}", bus.name))
                .changed()
            {
                out.push(StripRequest::Chain(ChainOp::Bypass { index: i, bypass }));
            }
            if ui
                .add_enabled(i > 0, egui::Button::new("Move up"))
                .on_hover_text(format!("Move {name} earlier in the chain"))
                .clicked()
            {
                out.push(StripRequest::Chain(ChainOp::Move { from: i, to: i - 1 }));
            }
            if ui
                .add_enabled(i < last, egui::Button::new("Move down"))
                .on_hover_text(format!("Move {name} later in the chain"))
                .clicked()
            {
                out.push(StripRequest::Chain(ChainOp::Move { from: i, to: i + 1 }));
            }
            if ui
                .button("Remove")
                .on_hover_text(format!("Remove {name} from {}", bus.name))
                .clicked()
            {
                out.push(StripRequest::Chain(ChainOp::Remove { index: i }));
            }
        });
    }
    ui.horizontal(|ui| {
        let combo = egui::ComboBox::from_id_salt(("audio-mixer-add-kind", &bus.name))
            .selected_text(kind_label(
                BUS_EFFECT_KINDS.get(*add_kind).copied().unwrap_or("eq"),
            ))
            .show_ui(ui, |ui| {
                for (k, kind) in BUS_EFFECT_KINDS.iter().enumerate() {
                    ui.selectable_value(add_kind, k, kind_label(kind));
                }
            });
        crate::a11y_audit::spoken_only(combo.response, egui::WidgetType::ComboBox, "Effect kind to add");
        if ui
            .add_enabled(bus.effects.len() < MAX_CHAIN, egui::Button::new("Add effect"))
            .clicked()
        {
            let kind = BUS_EFFECT_KINDS.get(*add_kind).copied().unwrap_or("eq");
            out.push(StripRequest::Chain(ChainOp::Add {
                kind: kind.to_owned(),
                index: None,
            }));
        }
    });
    if !panel.layout.send_buses.is_empty() {
        ui.label(egui::RichText::new("Sends").strong());
    }
    for send in &panel.layout.send_buses {
        ui.horizontal(|ui| {
            ui.label(format!("→ {}", send.name));
            match bus.sends.iter().find(|s| s.bus == send.name) {
                Some(route) => {
                    let mut db = route.volume_db;
                    let name = format!("Send level from {} to {}", bus.name, send.name);
                    let response = super::level_slider(ui, &mut db, SEND_MIN_DB..=FADER_MAX_DB, &name)
                        .on_hover_text(name);
                    if response.changed() {
                        out.push(StripRequest::Send(send.name.clone(), Some(db)));
                    }
                    let mut pre = route.tap == SendTap::PreFader;
                    if ui
                        .toggle_value(&mut pre, "Pre")
                        .on_hover_text(format!(
                            "Pre-fader: send {} to {} before its fader and mute (off: after them)",
                            bus.name, send.name
                        ))
                        .changed()
                    {
                        let tap = if pre {
                            SendTap::PreFader
                        } else {
                            SendTap::PostFader
                        };
                        out.push(StripRequest::Tap(send.name.clone(), tap));
                    }
                    if ui
                        .button("Remove send")
                        .on_hover_text(format!("Stop sending {} to {}", bus.name, send.name))
                        .clicked()
                    {
                        out.push(StripRequest::Send(send.name.clone(), None));
                    }
                }
                None => {
                    if ui
                        .button("Add send")
                        .on_hover_text(format!(
                            "Send {} to {} (silent until raised)",
                            bus.name, send.name
                        ))
                        .clicked()
                    {
                        out.push(StripRequest::Send(send.name.clone(), Some(SEND_MIN_DB)));
                    }
                }
            }
        });
    }
    out
}

/// What the snapshot list asked for this frame.
pub(super) enum SnapshotRequest {
    Op(SnapshotOp),
    Save,
}

/// The snapshot list: one row per snapshot (apply, blend time, delete), a release button and a
/// save row (name field, Save).
pub(super) fn snapshots_ui(
    ui: &mut egui::Ui,
    tokens: &crate::theme::ThemeTokens,
    panel: &AudioMixerPanel,
    new_name: &mut String,
) -> Vec<SnapshotRequest> {
    let mut out = Vec::new();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Snapshots").strong());
        match &panel.active_snapshot {
            Some(name) => {
                ui.label(egui::RichText::new(format!("Applied: {name}")).color(tokens.accents.active))
            }
            None => ui.label(egui::RichText::new("None applied").color(tokens.muted)),
        };
        if ui
            .add_enabled(!panel.applied.is_empty(), egui::Button::new("Release"))
            .on_hover_text("Blend back from the applied snapshot")
            .clicked()
        {
            out.push(SnapshotRequest::Op(SnapshotOp::Release));
        }
    });
    for snapshot in &panel.snapshots {
        ui.horizontal(|ui| {
            ui.label(&snapshot.name);
            if ui
                .button("Apply")
                .on_hover_text(format!("Blend to {} over its blend time", snapshot.name))
                .clicked()
            {
                out.push(SnapshotRequest::Op(SnapshotOp::Apply {
                    name: snapshot.name.clone(),
                }));
            }
            let mut secs = snapshot.blend_secs;
            let response = ui.add(
                egui::DragValue::new(&mut secs)
                    .range(0.0..=MAX_BLEND_SECS)
                    .speed(0.05)
                    .suffix(" s"),
            );
            let response = crate::a11y_audit::named_drag_value(
                response,
                &format!("Blend time of {}", snapshot.name),
                f64::from(secs),
            );
            if response.changed() {
                out.push(SnapshotRequest::Op(SnapshotOp::Blend {
                    name: snapshot.name.clone(),
                    blend_secs: secs,
                }));
            }
            if ui
                .button("Delete")
                .on_hover_text(format!("Delete snapshot {}", snapshot.name))
                .clicked()
            {
                out.push(SnapshotRequest::Op(SnapshotOp::Delete {
                    name: snapshot.name.clone(),
                }));
            }
        });
    }
    ui.horizontal(|ui| {
        let field = ui.add(
            egui::TextEdit::singleline(new_name)
                .hint_text("Snapshot name")
                .desired_width(140.0),
        );
        crate::a11y_audit::spoken_only(field, egui::WidgetType::TextEdit, "Snapshot name")
            .on_hover_text("Name of the snapshot to save");
        if ui
            .add_enabled(!new_name.trim().is_empty(), egui::Button::new("Save snapshot"))
            .on_hover_text("Save every fader, mute and send level as a snapshot")
            .clicked()
        {
            out.push(SnapshotRequest::Save);
        }
    });
    out
}

/// The display name of an effect kind.
fn kind_label(kind: &str) -> &'static str {
    match kind {
        "eq" => "EQ",
        "low_pass" => "Low-pass",
        "high_pass" => "High-pass",
        "reverb" => "Reverb",
        "delay" => "Delay",
        "compressor" => "Compressor",
        "distortion" => "Distortion",
        "convolution" => "Convolution",
        _ => "Effect",
    }
}

/// An effect's row label: its kind and its main setting.
fn effect_label(effect: &BusEffect) -> String {
    let base = kind_label(effect.kind());
    match effect.active() {
        BusEffect::LowPass { cutoff_hz, .. } | BusEffect::HighPass { cutoff_hz, .. } => {
            format!("{base} {cutoff_hz:.0} Hz")
        }
        BusEffect::Reverb { mix, .. } | BusEffect::Delay { mix, .. } => format!("{base} mix {mix:.2}"),
        BusEffect::Compressor {
            threshold_db, ratio, ..
        } => format!("{base} {threshold_db:.0} dB {ratio:.1}:1"),
        BusEffect::Distortion { drive_db } => format!("{base} {drive_db:.1} dB"),
        BusEffect::Convolution { ir, .. } if !ir.is_empty() => format!("{base} {ir}"),
        _ => base.to_owned(),
    }
}

// --------------------------------------------------------------------------------------- agent surface

/// The JSON of one effect: its kind, bypass and parameters.
pub(super) fn effect_json(effect: &BusEffect) -> Value {
    let mut out = serde_json::to_value(effect.active()).unwrap_or(Value::Null);
    // serde's externally tagged form `{ "LowPass": { .. } }` → its parameters.
    let params = out
        .as_object_mut()
        .and_then(|o| o.values_mut().next().map(std::mem::take))
        .unwrap_or_else(|| json!({}));
    json!({ "kind": effect.kind(), "bypassed": effect.is_bypassed(), "params": params })
}

/// The JSON of one snapshot.
pub(super) fn snapshot_json(snapshot: &AudioSnapshot) -> Value {
    json!({
        "name": snapshot.name,
        "blend_secs": snapshot.blend_secs,
        "bus_volumes": snapshot.bus_volumes,
        "bus_mutes": snapshot.bus_mutes,
        "send_volumes": snapshot.send_volumes.iter().map(|s| json!({
            "bus": s.bus, "send": s.send, "volume_db": s.volume_db,
        })).collect::<Vec<_>>(),
    })
}

fn req_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (a string) is required")))
}

fn opt_index(params: &Value, key: &str) -> Result<Option<usize>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a non-negative integer"))),
    }
}

fn req_index(params: &Value, key: &str) -> Result<usize, RpcError> {
    opt_index(params, key)?.ok_or_else(|| RpcError::invalid_params(format!("`{key}` is required")))
}

fn answer(world: &mut World, changed: bool) -> Value {
    let mut out = state_json(world);
    out["changed"] = json!(changed);
    out["undo_depth"] = json!(world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth));
    out
}

fn bad_op(op: &str, ops: &str) -> RpcError {
    PanelError::new("audio_mixer_bad_op", format!("unknown `op` `{op}` (ops: {ops})")).rpc(HINT)
}

fn mixer_chain(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bus = req_str(params, "bus")?.to_owned();
    let op = match req_str(params, "op")? {
        "add" => ChainOp::Add {
            kind: req_str(params, "kind")?.to_owned(),
            index: opt_index(params, "index")?,
        },
        "remove" => ChainOp::Remove {
            index: req_index(params, "index")?,
        },
        "move" => ChainOp::Move {
            from: req_index(params, "index")?,
            to: req_index(params, "to")?,
        },
        "bypass" => ChainOp::Bypass {
            index: req_index(params, "index")?,
            bypass: opt_bool(params, "bypass")?.unwrap_or(true),
        },
        other => return Err(bad_op(other, "add, remove, move, bypass")),
    };
    let changed = edit_chain(world, &bus, &op).map_err(|e| e.rpc(HINT))?;
    Ok(answer(world, changed))
}

fn mixer_send(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bus = req_str(params, "bus")?.to_owned();
    let send = req_str(params, "send")?.to_owned();
    let remove = opt_bool(params, "remove")?.unwrap_or(false);
    let tap = match params.get("tap") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) if t == "pre_fader" => Some(SendTap::PreFader),
        Some(Value::String(t)) if t == "post_fader" => Some(SendTap::PostFader),
        Some(other) => {
            return Err(PanelError::new(
                "audio_mixer_bad_value",
                format!("`tap` must be \"pre_fader\" or \"post_fader\", got {other}"),
            )
            .rpc(HINT));
        }
    };
    if let Some(tap) = tap {
        if remove {
            return Err(RpcError::invalid_params("pass `tap` or `remove: true`, not both"));
        }
        let volume_db = opt_f32(params, "volume_db")?;
        let changed = edit_send_tap(world, &bus, &send, tap, volume_db).map_err(|e| e.rpc(HINT))?;
        return Ok(answer(world, changed));
    }
    let volume_db = match (remove, opt_f32(params, "volume_db")?) {
        (true, _) => None,
        (false, Some(db)) => Some(db),
        (false, None) => {
            return Err(RpcError::invalid_params(
                "pass `volume_db` (a send level), `tap` or `remove: true`",
            ));
        }
    };
    let changed = edit_send(world, &bus, &send, volume_db).map_err(|e| e.rpc(HINT))?;
    Ok(answer(world, changed))
}

fn mixer_snapshot(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = || req_str(params, "name").map(str::to_owned);
    let op = match req_str(params, "op")? {
        "save" => SnapshotOp::Save {
            name: name()?,
            blend_secs: opt_f32(params, "blend_secs")?,
        },
        "blend" => SnapshotOp::Blend {
            name: name()?,
            blend_secs: opt_f32(params, "blend_secs")?
                .ok_or_else(|| RpcError::invalid_params("`blend_secs` is required"))?,
        },
        "delete" => SnapshotOp::Delete { name: name()? },
        "apply" => SnapshotOp::Apply { name: name()? },
        "release" => SnapshotOp::Release,
        other => return Err(bad_op(other, "save, blend, delete, apply, release")),
    };
    let changed = edit_snapshot(world, &op).map_err(|e| e.rpc(HINT))?;
    Ok(answer(world, changed))
}

fn mixer_save(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let path = save_layout(world).map_err(|e| e.rpc(HINT))?;
    let root = crate::mesh_source::project_root(world).unwrap_or_default();
    let shown = path.strip_prefix(&root).unwrap_or(&path).display().to_string();
    let mut out = answer(world, false);
    out["path"] = json!(shown.replace('\\', "/"));
    out["version"] = json!(regia_audio::AUDIO_BUSES_FORMAT_VERSION);
    Ok(out)
}

const CHAIN_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "bus",
        kind: "string",
        required: true,
        description: "Bus name (editor.audio_mixer_get → buses).",
    },
    ParamSchema {
        name: "op",
        kind: "string",
        required: true,
        description: "add | remove | move | bypass.",
    },
    ParamSchema {
        name: "kind",
        kind: "string",
        required: false,
        description: "add: eq | low_pass | high_pass | reverb | delay | compressor | distortion | convolution \
            (a near-neutral setting).",
    },
    ParamSchema {
        name: "index",
        kind: "integer",
        required: false,
        description: "The effect (0 = first). add: where it goes (default the end); move: the effect moved.",
    },
    ParamSchema {
        name: "to",
        kind: "integer",
        required: false,
        description: "move: the effect's new index.",
    },
    ParamSchema {
        name: "bypass",
        kind: "boolean",
        required: false,
        description: "bypass: true bypasses (default), false re-activates.",
    },
];

const SEND_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "bus",
        kind: "string",
        required: true,
        description: "The sending bus.",
    },
    ParamSchema {
        name: "send",
        kind: "string",
        required: true,
        description: "A send bus of the layout (editor.audio_mixer_get → send_buses).",
    },
    ParamSchema {
        name: "volume_db",
        kind: "number",
        required: false,
        description: "Send level, dB, -60..=6 (-60 is silent); adds the route when the bus has none.",
    },
    ParamSchema {
        name: "remove",
        kind: "boolean",
        required: false,
        description: "true removes the route.",
    },
    ParamSchema {
        name: "tap",
        kind: "string",
        required: false,
        description: "pre_fader | post_fader: where the send taps the bus (pre_fader: before its fader and mute). \
            With volume_db both change in one undo step; adds the route (silent unless volume_db) when the bus \
            has none. Omitted: the tap stays (a new route is post_fader).",
    },
];

const SNAPSHOT_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "op",
        kind: "string",
        required: true,
        description: "save | blend | delete | apply | release.",
    },
    ParamSchema {
        name: "name",
        kind: "string",
        required: false,
        description: "The snapshot (every op but release).",
    },
    ParamSchema {
        name: "blend_secs",
        kind: "number",
        required: false,
        description: "save, blend: the blend time, 0..=60 s (save default: the replaced snapshot's, else 0.5).",
    },
];

/// The chain, send, snapshot and save methods of the mixer dock.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.audio_mixer_chain",
                tool: "audio_mixer_chain",
                description: "Edit a bus's effect chain in the audio mixer dock: add an effect, remove, move or \
                    bypass one. One undo step; the pumped world's mixer takes the new chain at the next editor \
                    frame (a chain change rebuilds its buses and stops playing voices).",
                params: CHAIN_PARAMS,
                result: "{ changed, undo_depth, source, floor_db, buses: [{ effects: [{ kind, bypassed, params }], \
                    sends }], send_buses, snapshots }",
                mutating: true,
                permission: Permission::EditScene,
            },
            mixer_chain as Handler,
        ),
        (
            MethodSchema {
                name: "editor.audio_mixer_send",
                tool: "audio_mixer_send",
                description: "Set a bus's send level into a send bus (adding the route), its tap (pre_fader or \
                    post_fader, the dock's Pre toggle) or remove the route, as the dock's send knobs do. One undo \
                    step.",
                params: SEND_PARAMS,
                result: "{ changed, undo_depth, buses: [{ sends: [{ bus, volume_db, tap }] }] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            mixer_send as Handler,
        ),
        (
            MethodSchema {
                name: "editor.audio_mixer_snapshot",
                tool: "audio_mixer_snapshot",
                description: "Named mixer snapshots: save (every fader, mute and send level), blend (set the blend \
                    time), delete (each one undo step), apply (push it on the pumped world's mixer, blending over \
                    its blend time) and release (pop it).",
                params: SNAPSHOT_PARAMS,
                result: "{ changed, undo_depth, snapshots: [{ name, blend_secs, bus_volumes, bus_mutes, \
                    send_volumes }], active_snapshot, applied }",
                mutating: true,
                permission: Permission::EditScene,
            },
            mixer_snapshot as Handler,
        ),
        (
            MethodSchema {
                name: "editor.audio_mixer_save",
                tool: "audio_mixer_save",
                description: "Write the mixer's bus layout (faders, mutes, chains, sends, snapshots) to the \
                    project's config/audio.ron (audio-buses v1).",
                params: &[],
                result: "{ path, version, changed, undo_depth, buses, snapshots }",
                mutating: true,
                permission: Permission::EditScene,
            },
            mixer_save as Handler,
        ),
    ]
}
