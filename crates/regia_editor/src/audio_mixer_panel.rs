// SPDX-License-Identifier: MPL-2.0
//! The audio mixer dock (`docs/specs/16-audio.md` § 5, ADR-2530): the bus tree with a fader, mute and
//! solo per bus and a live meter (peak, RMS, short-term loudness) from the play-in-editor world.
//! Agent surface: `editor.audio_mixer_get` and `editor.audio_mixer_set` (`book/agents/audio-mixer.md`).
//!
//! Each strip also carries its effect chain and sends, and the dock keeps the layout's named
//! snapshots (`chain`, parity row P-227, ADR-8354): chain, send and snapshot edits are undo steps,
//! reach the pumped world through `Audio::set_bus_layout`, and Save writes `config/audio.ron`.
//!
//! The panel owns a mixer model (the bus tree with its fader, mute and solo). Every edit is one
//! [`MixerEdit`] on the editor's undo stack. Each editor frame [`pump`] sends the model to the
//! `regia_audio::Audio` of the play world (through `Audio::set_bus_volume` and `set_bus_mute`, the
//! same calls `audio.set_bus` uses) and reads the play world's bus meters back. Outside play, the
//! edit world's `Audio` (when it has one) is driven and read the same way.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_audio::bus_meter::{BusMeterReading, METER_FLOOR_DB};
use regia_audio::{Audio, AudioSnapshot, BusEffect, BusLayout, BusSend};
use regia_ecs::prelude::*;
use serde_json::{Value, json};
use std::time::Duration;

use crate::command::{EditorCommand, UndoStack};
use crate::data_panels::{PanelError, opt_f32};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/audio-mixer.md";

mod chain;
pub use chain::{
    ChainOp, LayoutEdit, LayoutState, MAX_BLEND_SECS, MAX_CHAIN, MAX_SNAPSHOTS, SEND_MIN_DB, SnapshotOp,
    edit_chain, edit_send, edit_send_tap, edit_snapshot, save_layout,
};

/// Fader range, dB.
pub const FADER_MIN_DB: f32 = -80.0;

/// Fader range top, dB.
pub const FADER_MAX_DB: f32 = 6.0;

/// Fade the pump asks of the mixer for a fader move, so a drag does not click.
const FADE: Duration = Duration::from_millis(30);

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("audio_mixer_{code}"), message)
}

/// Which world the meters came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    /// No world with an `Audio` resource: meters read silence.
    #[default]
    None,
    /// The edit world's `Audio`.
    Edit,
    /// The play-in-editor world's `Audio`.
    Play,
}

impl Source {
    /// `none`, `edit` or `play`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Edit => "edit",
            Self::Play => "play",
        }
    }
}

/// One bus of the mixer model.
#[derive(Clone, Debug, PartialEq)]
pub struct MixerBus {
    /// Bus name.
    pub name: String,
    /// Parent bus (`None` for the root).
    pub parent: Option<String>,
    /// The fader, dB.
    pub volume_db: f32,
    /// Muted by the user.
    pub muted: bool,
    /// Soloed by the user.
    pub solo: bool,
    /// The effect chain, in order (a bypassed effect is [`BusEffect::Bypassed`]).
    pub effects: Vec<BusEffect>,
    /// Sends into the layout's send buses (each post- or pre-fader, [`regia_audio::SendTap`]).
    pub sends: Vec<BusSend>,
}

/// The fader, mute and solo of one bus: what a [`MixerEdit`] swaps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BusState {
    /// The fader, dB.
    pub volume_db: f32,
    /// Muted.
    pub muted: bool,
    /// Soloed.
    pub solo: bool,
}

/// The mixer dock's state (a resource of the edit world).
#[derive(Resource, Debug, Default)]
pub struct AudioMixerPanel {
    buses: Vec<MixerBus>,
    meters: Vec<(String, BusMeterReading)>,
    /// The live program-loudness reading of the pumped world's output (ADR-7227).
    program: regia_audio::ProgramLoudness,
    /// A Reset press, applied at the next pump.
    reset_program: bool,
    source: Source,
    /// Bumped by every model change; [`pump`] re-sends the model when it moved.
    generation: u64,
    sent: Option<u64>,
    last_error: Option<String>,
    /// The layout the model was loaded from (send buses, medium bypass and everything the dock
    /// does not edit).
    layout: BusLayout,
    /// The named snapshots.
    snapshots: Vec<AudioSnapshot>,
    /// Bumped by every chain, send or snapshot change; 0 while the model is the loaded layout.
    layout_generation: u64,
    /// Bumped by every change [`save_layout`] writes (faders, mutes, chains, sends, snapshots).
    edits: u64,
    /// The `edits` count [`save_layout`] last wrote.
    saved_edits: u64,
    /// The layout generation and world the pump last sent the layout to.
    layout_sent: Option<(u64, Source)>,
    /// Snapshot auditions waiting for the next pump.
    auditions: Vec<chain::Audition>,
    /// The snapshots the dock applied, oldest first (Release pops the last).
    applied: Vec<String>,
    /// The pumped world's active snapshot.
    active_snapshot: Option<String>,
    /// The pumped world's live bus levels (the snapshot blend), dB.
    live_db: Vec<(String, f32)>,
    /// The strip whose chain editor is open.
    open_strip: Option<String>,
    /// The effect kind the Add row offers.
    add_kind: usize,
    /// The name field of the snapshot Save row.
    new_snapshot: String,
    /// The egui frame of the last dock action (a second UI pass of that frame does nothing).
    last_action_frame: Option<u64>,
}

impl AudioMixerPanel {
    /// The mixer model, parents first.
    #[must_use]
    pub fn buses(&self) -> &[MixerBus] {
        &self.buses
    }

    /// The bus `name`.
    #[must_use]
    pub fn bus(&self, name: &str) -> Option<&MixerBus> {
        self.buses.iter().find(|b| b.name == name)
    }

    /// The latest meter reading of `name`, floored; silence when none was read.
    #[must_use]
    pub fn meter(&self, name: &str) -> BusMeterReading {
        self.meters
            .iter()
            .find(|(n, _)| n == name)
            .map_or(BusMeterReading::SILENT, |(_, r)| *r)
            .floored()
    }

    /// Whether the bus is audible under the current solos: with no solo every unmuted bus is
    /// audible; with a solo only soloed buses and their ancestors and descendants are, and a muted
    /// bus never is.
    #[must_use]
    pub fn effective_muted(&self, name: &str) -> bool {
        let Some(bus) = self.bus(name) else {
            return false;
        };
        if bus.muted {
            return true;
        }
        if !self.buses.iter().any(|b| b.solo) {
            return false;
        }
        // Follow the chain to the root: a soloed bus on it keeps this bus (a descendant) audible.
        let mut cur = Some(bus);
        let mut hops = 0;
        while let Some(b) = cur {
            if b.solo {
                return false;
            }
            hops += 1;
            if hops > self.buses.len() {
                break;
            }
            cur = b.parent.as_deref().and_then(|p| self.bus(p));
        }
        // An ancestor of a soloed bus stays audible too, or the solo would be silent.
        let is_ancestor_of_solo = self.buses.iter().filter(|b| b.solo).any(|solo| {
            let mut cur = solo.parent.as_deref();
            let mut hops = 0;
            while let Some(p) = cur {
                if p == name {
                    return true;
                }
                hops += 1;
                if hops > self.buses.len() {
                    break;
                }
                cur = self.bus(p).and_then(|b| b.parent.as_deref());
            }
            false
        });
        !is_ancestor_of_solo
    }

    /// The latest program-loudness reading of the output (momentary, short-term, integrated,
    /// range, true peak; ADR-7227); empty when no world has audio.
    #[must_use]
    pub fn program_loudness(&self) -> regia_audio::ProgramLoudness {
        self.program
    }

    /// Starts a new program-loudness measurement at the next pump (the dock's Reset button).
    pub fn request_program_reset(&mut self) {
        self.reset_program = true;
    }

    /// The message of the last failed edit, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// The named snapshots.
    #[must_use]
    pub fn snapshots(&self) -> &[AudioSnapshot] {
        &self.snapshots
    }

    /// The pumped world's active snapshot, as of the last pump.
    #[must_use]
    pub fn active_snapshot(&self) -> Option<&str> {
        self.active_snapshot.as_deref()
    }

    /// The live level of `bus` in the pumped world (the snapshot blend), dB, as of the last pump.
    #[must_use]
    pub fn live_db(&self, bus: &str) -> Option<f32> {
        self.live_db.iter().find(|(n, _)| n == bus).map(|(_, db)| *db)
    }

    /// Whether fader, mute, chain, send or snapshot edits are not yet written to `config/audio.ron`.
    #[must_use]
    pub fn unsaved(&self) -> bool {
        self.edits != self.saved_edits
    }

    /// The chain, send and snapshot part of the model.
    #[must_use]
    pub fn layout_state(&self) -> LayoutState {
        LayoutState {
            buses: self.buses.clone(),
            snapshots: self.snapshots.clone(),
        }
    }

    /// The bus layout the model describes now: the loaded layout with every bus's fader, mute,
    /// chain and sends and the snapshots taken from the model.
    #[must_use]
    pub fn layout_now(&self) -> BusLayout {
        let mut layout = self.layout.clone();
        for def in &mut layout.buses {
            if let Some(bus) = self.bus(&def.name) {
                def.volume_db = bus.volume_db;
                def.mute = bus.muted;
                def.effects = bus.effects.clone();
                def.sends = bus.sends.clone();
            }
        }
        layout.snapshots = self.snapshots.clone();
        layout.version = regia_audio::AUDIO_BUSES_FORMAT_VERSION;
        layout
    }
}

/// Builds the model from a layout.
fn model_of(layout: &BusLayout) -> Vec<MixerBus> {
    layout
        .buses
        .iter()
        .map(|b| MixerBus {
            name: b.name.clone(),
            parent: b.parent.clone(),
            volume_db: b.volume_db,
            muted: b.mute,
            solo: false,
            effects: b.effects.clone(),
            sends: b.sends.clone(),
        })
        .collect()
}

/// The layout the project authored (`config/audio.ron`, migrated to the current `audio-buses`
/// version), else the default tree; the error says why a file present was not used.
fn project_layout(world: &World) -> (BusLayout, Option<String>) {
    let Some(text) = crate::mesh_source::project_root(world)
        .and_then(|root| std::fs::read_to_string(root.join(crate::creators::AUDIO_CONFIG_PATH)).ok())
    else {
        return (BusLayout::default_tree(), None);
    };
    match BusLayout::parse(&text) {
        Ok(layout) => (layout, None),
        Err(e) => (
            BusLayout::default_tree(),
            Some(format!("{e}; showing the default tree")),
        ),
    }
}

/// The panel with its model loaded (first use), taken out of the world.
fn take_panel(world: &mut World) -> AudioMixerPanel {
    let mut panel = world.remove_resource::<AudioMixerPanel>().unwrap_or_default();
    if panel.buses.is_empty() {
        let (layout, error) = project_layout(world);
        panel.buses = model_of(&layout);
        panel.snapshots = layout.snapshots.clone();
        panel.layout = layout;
        panel.last_error = error.or(panel.last_error);
        panel.generation += 1;
    }
    panel
}

/// One reversible fader, mute or solo change (one undo step).
pub struct MixerEdit {
    bus: String,
    before: BusState,
    after: BusState,
}

impl MixerEdit {
    fn set(world: &mut World, bus: &str, state: BusState) {
        let mut panel = take_panel(world);
        if let Some(b) = panel.buses.iter_mut().find(|b| b.name == bus) {
            if b.volume_db != state.volume_db || b.muted != state.muted {
                panel.edits += 1;
            }
            b.volume_db = state.volume_db;
            b.muted = state.muted;
            b.solo = state.solo;
            panel.generation += 1;
        }
        world.insert_resource(panel);
    }
}

impl EditorCommand for MixerEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, &self.bus, self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, &self.bus, self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<MixerEdit>() else {
            return false;
        };
        // A fader scrub: consecutive volume-only moves of one bus are one step.
        let volume_only = |e: &MixerEdit| e.before.muted == e.after.muted && e.before.solo == e.after.solo;
        if self.bus == next.bus && volume_only(self) && volume_only(next) {
            self.after = next.after;
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        let (b, a) = (self.before, self.after);
        let what = if a.muted != b.muted {
            if a.muted { "mute" } else { "unmute" }
        } else if a.solo != b.solo {
            if a.solo { "solo" } else { "unsolo" }
        } else {
            "volume"
        };
        format!("Mixer: {what} {}", self.bus)
    }
}

/// What an edit asks for; `None` keeps the field.
#[derive(Clone, Copy, Debug, Default)]
pub struct EditRequest {
    /// New fader, dB.
    pub volume_db: Option<f32>,
    /// New mute.
    pub mute: Option<bool>,
    /// New solo.
    pub solo: Option<bool>,
}

/// Applies an edit as one undo step. `Ok(false)` when nothing changed (no undo entry).
///
/// # Errors
///
/// `audio_mixer_unknown_bus` for a bus the model lacks; `audio_mixer_bad_value` for a non-finite or
/// out-of-range fader.
pub fn edit_bus(world: &mut World, bus: &str, req: EditRequest) -> Result<bool, PanelError> {
    let panel = take_panel(world);
    let current = panel.bus(bus).map(|b| BusState {
        volume_db: b.volume_db,
        muted: b.muted,
        solo: b.solo,
    });
    let names = panel.buses.iter().map(|b| b.name.clone()).collect::<Vec<_>>();
    world.insert_resource(panel);
    let Some(before) = current else {
        return Err(err(
            "unknown_bus",
            format!("no bus named `{bus}` (buses: {})", names.join(", ")),
        ));
    };
    if let Some(db) = req.volume_db
        && (!db.is_finite() || !(FADER_MIN_DB..=FADER_MAX_DB).contains(&db))
    {
        return Err(err(
            "bad_value",
            format!("`volume_db` must be within {FADER_MIN_DB}..={FADER_MAX_DB}, got {db}"),
        ));
    }
    let after = BusState {
        volume_db: req.volume_db.unwrap_or(before.volume_db),
        muted: req.mute.unwrap_or(before.muted),
        solo: req.solo.unwrap_or(before.solo),
    };
    if after == before {
        return Ok(false);
    }
    let mut command = MixerEdit {
        bus: bus.to_owned(),
        before,
        after,
    };
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    Ok(true)
}

/// Sends the model to `audio` and reads its meters.
fn pump_world(panel: &mut AudioMixerPanel, world: &mut World, source: Source) {
    let _span = tracing::trace_span!(target: "regia_editor", "audio_mixer_pump").entered();
    let mut resend = panel.sent != Some(panel.generation);
    let Some(mut audio) = world.get_resource_mut::<Audio>() else {
        panel.meters.clear();
        panel.program = regia_audio::ProgramLoudness::EMPTY;
        panel.source = Source::None;
        panel.live_db.clear();
        panel.active_snapshot = None;
        return;
    };
    // Chain, send and snapshot edits reach the mixer as a whole layout (ADR-8354); a rebuild
    // starts every bus at its layout level, so the faders are sent again after it.
    if panel.layout_generation > 0 && panel.layout_sent != Some((panel.layout_generation, source)) {
        if let Err(e) = audio.set_bus_layout(panel.layout_now()) {
            panel.last_error = Some(format!("the mixer refused the layout: {e}"));
        }
        panel.layout_sent = Some((panel.layout_generation, source));
        resend = true;
    }
    if resend || panel.source != source {
        for bus in &panel.buses {
            if audio.set_bus_volume(&bus.name, bus.volume_db, FADE).is_err() {
                continue;
            }
            let muted = panel.effective_muted(&bus.name);
            let _ = audio.set_bus_mute(&bus.name, muted);
        }
        panel.sent = Some(panel.generation);
    }
    for audition in std::mem::take(&mut panel.auditions) {
        let result = match audition {
            chain::Audition::Apply(snapshot) => {
                let name = snapshot.name.clone();
                audio.register_snapshot(snapshot);
                audio.apply_snapshot(&name)
            }
            chain::Audition::Release(secs) => {
                audio.pop_snapshot(Duration::try_from_secs_f32(secs).unwrap_or(Duration::ZERO))
            }
        };
        if let Err(e) = result {
            panel.last_error = Some(format!("snapshot: {e}"));
        }
    }
    panel.active_snapshot = audio.active_snapshot().map(str::to_owned);
    panel.live_db = audio
        .bus_status()
        .into_iter()
        .map(|b| (b.name, b.current_db))
        .collect();
    panel.meters = audio.bus_meters();
    // The dock's program meter: on while the dock pumps this world (a new world starts a new
    // measurement), reset on request.
    audio.set_program_loudness_enabled(true);
    if std::mem::take(&mut panel.reset_program) || panel.source != source {
        audio.reset_program_loudness();
    }
    panel.program = audio.program_loudness();
    panel.source = source;
}

/// Each editor frame: drives the play world's `Audio` while a session runs (else the edit world's)
/// from the model and refreshes the meters.
pub fn pump(world: &mut World, session: Option<&mut crate::pie::PieSession>) {
    let Some(mut panel) = world.remove_resource::<AudioMixerPanel>() else {
        return;
    };
    match session {
        Some(session) => pump_world(&mut panel, session.app.world_mut(), Source::Play),
        None => pump_world(&mut panel, world, Source::Edit),
    }
    world.insert_resource(panel);
}

/// Names a slider for screen readers (the a11y audit's named-widget rule) and keeps its value.
fn named_slider(response: egui::Response, name: &str, value: f32) -> egui::Response {
    let enabled = response.enabled();
    response.widget_info(|| egui::WidgetInfo::slider(enabled, f64::from(value), name));
    response
}

/// A dB level slider named `name` with its typed value box beside it, named `<name> value`
/// (egui's built-in value box of a slider has no accessible name). The returned response is
/// the slider's, marked changed when either widget changed `db`.
fn level_slider(
    ui: &mut egui::Ui,
    db: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    name: &str,
) -> egui::Response {
    let slider = ui.add(egui::Slider::new(db, range.clone()).show_value(false));
    let mut slider = named_slider(slider, name, *db);
    let typed = ui.add(
        egui::DragValue::new(db)
            .range(range)
            .speed(0.1)
            .suffix(" dB")
            .fixed_decimals(1),
    );
    let typed = crate::a11y_audit::named_drag_value(typed, &format!("{name} value"), f64::from(*db));
    if typed.changed() {
        slider.mark_changed();
    }
    slider
}

/// A meter or fader level as people read it: `−∞ dB` at the floor (bug 25, ADR-8586).
fn db_text(db: f32) -> String {
    if db <= METER_FLOOR_DB {
        "−∞ dB".to_owned()
    } else {
        format!("{db:.1} dB")
    }
}

/// A horizontal level bar from `floor` up to 0 dB, with the value left of it. Track and bar colors
/// are the theme's (ADR-8589): the recessed `well` fill, then `accents.alert` (clipping),
/// `accents.unsaved` (the warning color, hot) and `accents.ok`.
fn meter_bar(ui: &mut egui::Ui, label: &str, db: f32) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    ui.horizontal(|ui| {
        ui.add_sized([34.0, 12.0], egui::Label::new(egui::RichText::new(label).small()));
        let (rect, _) = ui.allocate_exact_size(egui::vec2(120.0, 10.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 2.0, tokens.well);
        let frac = ((db - -60.0) / 60.0).clamp(0.0, 1.0);
        let color = if db > -1.0 {
            tokens.accents.alert
        } else if db > -9.0 {
            tokens.accents.unsaved
        } else {
            tokens.accents.ok
        };
        if frac > 0.0 {
            let mut fill = rect;
            fill.set_width(rect.width() * frac);
            ui.painter().rect_filled(fill, 2.0, color);
        }
        ui.label(egui::RichText::new(db_text(db)).small().monospace());
    });
}

/// The program-loudness rows (EBU R128 of the whole output); returns true when Reset was pressed.
fn program_rows(ui: &mut egui::Ui, program: &regia_audio::ProgramLoudness) -> bool {
    let mut reset = false;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Program").strong());
        ui.label(
            egui::RichText::new(format!("{:.1} s", program.seconds))
                .small()
                .monospace(),
        );
        reset = ui.small_button("Reset").clicked();
    });
    let floor = |x: f32| if x.is_finite() { x } else { METER_FLOOR_DB };
    ui.indent("audio-mixer-program", |ui| {
        meter_bar(ui, "M", floor(program.momentary_lufs));
        meter_bar(ui, "S", floor(program.short_term_lufs));
        meter_bar(ui, "I", floor(program.integrated_lufs));
        meter_bar(ui, "TP", floor(program.true_peak_db));
        ui.label(
            egui::RichText::new(format!("LRA {:.1} LU", program.loudness_range_lu))
                .small()
                .monospace(),
        );
    });
    reset
}

/// Draws the dock.
pub fn audio_mixer_panel(ui: &mut egui::Ui, world: &mut World) {
    let panel = take_panel(world);
    let rows: Vec<(usize, MixerBus, bool, BusMeterReading)> = {
        let mut out = Vec::new();
        fn walk(
            panel: &AudioMixerPanel,
            parent: Option<&str>,
            depth: usize,
            out: &mut Vec<(usize, MixerBus, bool, BusMeterReading)>,
        ) {
            for b in panel.buses.iter().filter(|b| b.parent.as_deref() == parent) {
                if depth > panel.buses.len() {
                    return;
                }
                out.push((
                    depth,
                    b.clone(),
                    panel.effective_muted(&b.name),
                    panel.meter(&b.name),
                ));
                walk(panel, Some(&b.name), depth + 1, out);
            }
        }
        walk(&panel, None, 0, &mut out);
        out
    };
    let source = panel.source;
    let last_error = panel.last_error.clone();
    let program = panel.program;
    let frame = ui.ctx().cumulative_frame_nr();
    let acted = panel.last_action_frame == Some(frame);
    let tokens = crate::theme::tokens(world).clone();
    let mut open_strip = panel.open_strip.clone();
    let mut add_kind = panel.add_kind;
    let mut new_snapshot = panel.new_snapshot.clone();
    let mut strip_requests: Vec<(String, chain::StripRequest)> = Vec::new();
    let mut snapshot_requests: Vec<chain::SnapshotRequest> = Vec::new();
    let mut save_pressed = false;
    let unsaved = panel.unsaved();
    world.insert_resource(panel);

    ui.horizontal(|ui| {
        ui.label(format!("Meters: {}", source.name()));
        if source == Source::None {
            ui.weak("(press Play to see live levels)");
        }
    });
    if let Some(e) = last_error {
        ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e);
    }
    if program_rows(ui, &program) {
        let mut panel = take_panel(world);
        panel.request_program_reset();
        world.insert_resource(panel);
    }
    let mut requests: Vec<(String, EditRequest)> = Vec::new();
    egui::ScrollArea::vertical()
        .id_salt("audio-mixer-scroll")
        .show(ui, |ui| {
            for (depth, bus, effective_muted, meter) in &rows {
                ui.horizontal(|ui| {
                    ui.add_space(14.0 * *depth as f32);
                    ui.add_sized(
                        [70.0, 18.0],
                        egui::Label::new(egui::RichText::new(&bus.name).strong()),
                    );
                    let mut db = bus.volume_db;
                    let response = level_slider(
                        ui,
                        &mut db,
                        FADER_MIN_DB..=FADER_MAX_DB,
                        &format!("{} fader", bus.name),
                    );
                    if response.changed() {
                        requests.push((
                            bus.name.clone(),
                            EditRequest {
                                volume_db: Some(db),
                                ..EditRequest::default()
                            },
                        ));
                    }
                    let mut mute = bus.muted;
                    if ui.toggle_value(&mut mute, "M").changed() {
                        requests.push((
                            bus.name.clone(),
                            EditRequest {
                                mute: Some(mute),
                                ..EditRequest::default()
                            },
                        ));
                    }
                    let mut solo = bus.solo;
                    if ui.toggle_value(&mut solo, "S").changed() {
                        requests.push((
                            bus.name.clone(),
                            EditRequest {
                                solo: Some(solo),
                                ..EditRequest::default()
                            },
                        ));
                    }
                    let mut open = open_strip.as_deref() == Some(bus.name.as_str());
                    if ui
                        .toggle_value(&mut open, "Chain")
                        .on_hover_text(format!(
                            "Effect chain and sends of {} ({} effects)",
                            bus.name,
                            bus.effects.len()
                        ))
                        .changed()
                    {
                        open_strip = open.then(|| bus.name.clone());
                    }
                    if *effective_muted && !bus.muted {
                        ui.weak("(solo)");
                    }
                });
                if open_strip.as_deref() == Some(bus.name.as_str()) {
                    let panel = world.resource::<AudioMixerPanel>();
                    ui.indent(("audio-mixer-chain", &bus.name), |ui| {
                        for request in chain::strip_chain_ui(ui, &tokens, panel, bus, &mut add_kind) {
                            strip_requests.push((bus.name.clone(), request));
                        }
                    });
                }
                ui.indent(("audio-mixer-meter", &bus.name), |ui| {
                    meter_bar(ui, "peak", meter.peak_db);
                    meter_bar(ui, "rms", meter.rms_db);
                    meter_bar(ui, "LUFS", meter.short_term_lufs);
                });
            }
            ui.separator();
            let panel = world.resource::<AudioMixerPanel>();
            snapshot_requests = chain::snapshots_ui(ui, &tokens, panel, &mut new_snapshot);
            ui.horizontal(|ui| {
                save_pressed = ui
                    .button("Save layout")
                    .on_hover_text("Write the faders, mutes, chains, sends and snapshots to config/audio.ron")
                    .clicked();
                if unsaved {
                    ui.label(egui::RichText::new("· unsaved changes").color(tokens.accents.unsaved));
                }
            });
        });
    for (bus, req) in requests {
        if let Err(e) = edit_bus(world, &bus, req) {
            let mut panel = take_panel(world);
            panel.last_error = Some(e.message);
            world.insert_resource(panel);
        }
    }
    let mut panel = take_panel(world);
    panel.open_strip = open_strip;
    panel.add_kind = add_kind;
    panel.new_snapshot = new_snapshot;
    let any = !strip_requests.is_empty() || !snapshot_requests.is_empty() || save_pressed;
    if any && !acted {
        panel.last_action_frame = Some(frame);
    }
    world.insert_resource(panel);
    // A second UI pass of the same egui frame sees the same clicks: act once.
    if acted || !any {
        return;
    }
    let mut errors = Vec::new();
    for (bus, request) in strip_requests {
        let result = match request {
            chain::StripRequest::Chain(op) => edit_chain(world, &bus, &op),
            chain::StripRequest::Send(send, db) => edit_send(world, &bus, &send, db),
            chain::StripRequest::Tap(send, tap) => edit_send_tap(world, &bus, &send, tap, None),
        };
        errors.extend(result.err());
    }
    for request in snapshot_requests {
        let result = match request {
            chain::SnapshotRequest::Op(op) => edit_snapshot(world, &op),
            chain::SnapshotRequest::Save => {
                let name = world.resource::<AudioMixerPanel>().new_snapshot.trim().to_owned();
                let result = edit_snapshot(
                    world,
                    &SnapshotOp::Save {
                        name,
                        blend_secs: None,
                    },
                );
                if result.is_ok() {
                    world.resource_mut::<AudioMixerPanel>().new_snapshot.clear();
                }
                result
            }
        };
        errors.extend(result.err());
    }
    if save_pressed {
        errors.extend(save_layout(world).err());
    }
    let mut panel = take_panel(world);
    panel.last_error = errors.pop().map(|e| e.message).or(panel.last_error);
    world.insert_resource(panel);
}

// --------------------------------------------------------------------------------------- agent surface

fn meter_json(r: BusMeterReading) -> Value {
    let r = r.floored();
    json!({ "peak_db": r.peak_db, "rms_db": r.rms_db, "short_term_lufs": r.short_term_lufs })
}

/// The panel's state as `editor.audio_mixer_get` answers it.
fn state_json(world: &mut World) -> Value {
    let panel = take_panel(world);
    let buses: Vec<Value> = panel
        .buses
        .iter()
        .map(|b| {
            json!({
                "name": b.name,
                "parent": b.parent,
                "volume_db": b.volume_db,
                "muted": b.muted,
                "solo": b.solo,
                "effective_muted": panel.effective_muted(&b.name),
                "meter": meter_json(panel.meter(&b.name)),
                "live_db": panel.live_db(&b.name),
                "effects": b.effects.iter().map(chain::effect_json).collect::<Vec<_>>(),
                "sends": b.sends.iter().map(|s| json!({
                    "bus": s.bus, "volume_db": s.volume_db, "tap": chain::tap_name(s.tap),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let p = panel.program;
    let figure = |x: f32| if x.is_finite() { json!(x) } else { Value::Null };
    let out = json!({
        "source": panel.source.name(),
        "floor_db": METER_FLOOR_DB,
        "buses": buses,
        "program": {
            "enabled": p.enabled,
            "seconds": p.seconds,
            "momentary_lufs": figure(p.momentary_lufs),
            "short_term_lufs": figure(p.short_term_lufs),
            "integrated_lufs": figure(p.integrated_lufs),
            "loudness_range_lu": figure(p.loudness_range_lu),
            "true_peak_db": figure(p.true_peak_db),
        },
        "send_buses": panel.layout.send_buses.iter().map(|s| json!({
            "name": s.name,
            "effects": s.effects.iter().map(chain::effect_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "snapshots": panel.snapshots.iter().map(chain::snapshot_json).collect::<Vec<_>>(),
        "active_snapshot": panel.active_snapshot,
        "applied": panel.applied,
        "unsaved": panel.unsaved(),
        "version": regia_audio::AUDIO_BUSES_FORMAT_VERSION,
    });
    world.insert_resource(panel);
    out
}

fn mixer_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    // Outside play, with an Audio in the edit world, read it now so the answer is fresh.
    let playing = world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|s| s.mode.is_some());
    if !playing {
        let mut panel = take_panel(world);
        pump_world(&mut panel, world, Source::Edit);
        world.insert_resource(panel);
    }
    Ok(state_json(world))
}

fn mixer_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bus = params
        .get("bus")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`bus` (a bus name) is required"))?
        .to_owned();
    let mute = match params.get("mute") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`mute` must be true or false"))?,
        ),
    };
    let solo = match params.get("solo") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`solo` must be true or false"))?,
        ),
    };
    let req = EditRequest {
        volume_db: opt_f32(params, "volume_db")?,
        mute,
        solo,
    };
    let changed = edit_bus(world, &bus, req).map_err(|e| e.rpc(HINT))?;
    let mut out = state_json(world);
    out["changed"] = json!(changed);
    out["undo_depth"] = json!(world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth));
    Ok(out)
}

const STATE: &str = "{ source: none|edit|play, floor_db, buses: [{ name, parent, volume_db, muted, solo, \
    effective_muted, meter: { peak_db, rms_db, short_term_lufs }, live_db, effects: [{ kind, bypassed, params }], \
    sends: [{ bus, volume_db, tap }] }], program: { enabled, seconds, momentary_lufs, short_term_lufs, integrated_lufs, \
    loudness_range_lu, true_peak_db }, send_buses: [{ name, effects }], snapshots: [{ name, blend_secs, \
    bus_volumes, bus_mutes, send_volumes }], active_snapshot, applied, unsaved, version }";

const SET_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "bus",
        kind: "string",
        required: true,
        description: "Bus name (editor.audio_mixer_get → buses).",
    },
    ParamSchema {
        name: "volume_db",
        kind: "number",
        required: false,
        description: "Fader, dB, -80..=6.",
    },
    ParamSchema {
        name: "mute",
        kind: "boolean",
        required: false,
        description: "Mute (true) or unmute.",
    },
    ParamSchema {
        name: "solo",
        kind: "boolean",
        required: false,
        description: "Solo (true) or unsolo: with any solo, only soloed buses, their ancestors and descendants sound.",
    },
];

/// The mixer dock's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.audio_mixer_get",
                tool: "audio_mixer_get",
                description: "The audio mixer dock (spec 16 § 5): the bus tree with fader, mute, solo and the \
                    live meter (peak, RMS, short-term loudness; silence reads the -100 dB floor) from the play \
                    world while playing, and the program loudness of the whole output (momentary, short-term \
                    and integrated LUFS, loudness range, true peak; null before anything is measured).",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            mixer_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.audio_mixer_set",
                tool: "audio_mixer_set",
                description: "Move a bus fader and/or set its mute or solo, as the dock does: one undo step; \
                    the change reaches the play world's mixer at the next editor frame.",
                params: SET_PARAMS,
                result: "{ changed, undo_depth, source, floor_db, buses }",
                mutating: true,
                permission: Permission::EditScene,
            },
            mixer_set as Handler,
        ),
    ]
    .into_iter()
    .chain(chain::methods())
    .collect()
}

#[cfg(test)]
mod tests;
