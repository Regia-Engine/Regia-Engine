// SPDX-License-Identifier: MPL-2.0
//! The sound cue editor (`docs/specs/16-audio.md` § 5, ADR-2530): a `.cue.ron` cue's variations with weights,
//! pick mode, steal policy, volume and pitch ranges, low-pass filter and caption, with an audition button.
//! Agent surface: `editor.cue_edit` (`book/agents/audio-mixer.md`).
//!
//! The panel reads and writes the file through `regia_audio::SoundCueDef::from_ron` and `to_ron`, the
//! serializer the runtime loader uses; the `.cue.ron` shape is unchanged (version 3). Every change is one
//! [`CueEdit`] on the editor's undo stack. A cue nobody edited is never rewritten: saving it leaves the
//! file's bytes as they were.

use std::path::{Path, PathBuf};
use std::time::Duration;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_audio::{CueCaptionDef, CueCaptionKind, CuePlayMode, SoundCueDef, StealPolicy};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack};
use crate::data_panels::{PanelError, confined, write_atomic};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/audio-mixer.md";

/// Most variations a cue may hold in the editor.
pub const MAX_SOUNDS: usize = 256;

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("cue_editor_{code}"), message)
}

/// One change to a cue: what a widget or `editor.cue_edit` asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum CueChange {
    /// The pick mode.
    Mode(CuePlayMode),
    /// Never repeat the last N picks.
    NoRepeatLast(usize),
    /// The pitch ratio range `(min, max)`.
    Pitch(f32, f32),
    /// The random volume range `(min, max)` in dB.
    VolumeRange(f32, f32),
    /// The instance limit (`None` = unlimited).
    MaxConcurrent(Option<usize>),
    /// The steal policy.
    Steal(StealPolicy),
    /// The cooldown in milliseconds.
    CooldownMs(u64),
    /// The base volume in dB.
    BaseVolume(f32),
    /// The bus (`None` = Master).
    Bus(Option<String>),
    /// The low-pass filter: the base cutoff (`None` = no filter) and the octave offset.
    Filter {
        /// Base cutoff in Hz; `None` filters nothing.
        cutoff_hz: Option<f32>,
        /// Octaves added to the cutoff.
        offset_octaves: f32,
    },
    /// The caption a play sends (`None` = none).
    Caption(Option<CueCaptionDef>),
    /// Adds a variation `(sound path, weight)`.
    AddSound(String, f32),
    /// Removes the variation at an index.
    RemoveSound(usize),
    /// Sets the weight of the variation at an index.
    Weight(usize, f32),
}

impl CueChange {
    /// Coalescing key: consecutive changes with one key (a slider scrub) are one undo step.
    fn key(&self) -> Option<String> {
        match self {
            Self::Pitch(..) => Some("pitch".into()),
            Self::VolumeRange(..) => Some("volume_range".into()),
            Self::BaseVolume(_) => Some("base_volume".into()),
            Self::Filter { .. } => Some("filter".into()),
            Self::Weight(i, _) => Some(format!("weight:{i}")),
            _ => None,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Mode(_) => "mode",
            Self::NoRepeatLast(_) => "no-repeat",
            Self::Pitch(..) => "pitch range",
            Self::VolumeRange(..) => "volume range",
            Self::MaxConcurrent(_) => "instance limit",
            Self::Steal(_) => "steal policy",
            Self::CooldownMs(_) => "cooldown",
            Self::BaseVolume(_) => "volume",
            Self::Bus(_) => "bus",
            Self::Filter { .. } => "filter",
            Self::Caption(_) => "caption",
            Self::AddSound(..) => "add variation",
            Self::RemoveSound(_) => "remove variation",
            Self::Weight(..) => "weight",
        }
    }
}

fn finite(v: f32, what: &str) -> Result<f32, PanelError> {
    if v.is_finite() {
        Ok(v)
    } else {
        Err(err("bad_value", format!("`{what}` must be a finite number")))
    }
}

/// A caption as `editor.cue_edit` takes it: `{ key, kind?, priority?, planned_ms? }`.
///
/// # Errors
/// `cue_editor_bad_value` for an empty key, a non-finite cutoff or a bad number field.
fn caption_of(params: &Value) -> Result<CueCaptionDef, PanelError> {
    let map = params
        .get("caption")
        .ok_or_else(|| err("bad_value", "`caption` must be an object or null"))?;
    let Value::Object(map) = map else {
        return Err(err("bad_value", "`caption` must be an object or null"));
    };
    let key = map
        .get("key")
        .and_then(Value::as_str)
        .map(str::trim)
        .ok_or_else(|| err("bad_value", "`caption.key` (a Fluent key) is required"))?;
    if key.is_empty() {
        return Err(err("bad_value", "`caption.key` must not be empty"));
    }
    let kind = match map.get("kind") {
        None => CueCaptionKind::Sound,
        Some(Value::String(text)) => caption_kind(text)?,
        Some(_) => {
            return Err(err(
                "bad_value",
                "`caption.kind` must be sound, speech or narration",
            ));
        }
    };
    let number = |name: &str| -> Result<f64, PanelError> {
        map.get(name)
            .and_then(Value::as_f64)
            .ok_or_else(|| err("bad_value", format!("`caption.{name}` must be a number")))
    };
    let priority = match map.get("priority") {
        None => 0,
        Some(_) => number("priority")? as u32,
    };
    let planned_ms = match map.get("planned_ms") {
        None | Some(Value::Null) => None,
        Some(_) => Some(number("planned_ms")? as u32),
    };
    Ok(CueCaptionDef {
        key: key.to_owned(),
        kind,
        priority: u8::try_from(priority)
            .map_err(|_| err("bad_value", "`caption.priority` must be 0 to 255"))?,
        planned_ms,
    })
}

/// A caption kind by name (`sound`, `speech`, `narration`).
///
/// # Errors
/// `cue_editor_bad_value` for any other name.
fn caption_kind(text: &str) -> Result<CueCaptionKind, PanelError> {
    match text {
        "sound" => Ok(CueCaptionKind::Sound),
        "speech" => Ok(CueCaptionKind::Speech),
        "narration" => Ok(CueCaptionKind::Narration),
        other => Err(err(
            "bad_value",
            format!("`caption.kind` must be sound, speech or narration, not `{other}`"),
        )),
    }
}

fn caption_kind_name(kind: &CueCaptionKind) -> &'static str {
    match kind {
        CueCaptionKind::Sound => "sound",
        CueCaptionKind::Speech => "speech",
        CueCaptionKind::Narration => "narration",
    }
}

/// Applies `change` to `def`, validating it.
///
/// # Errors
///
/// `cue_editor_bad_value` for a non-finite, negative or inverted value; `cue_editor_bad_index` for a variation
/// that does not exist; `cue_editor_too_many_sounds` past [`MAX_SOUNDS`].
pub fn apply_change(def: &mut SoundCueDef, change: &CueChange) -> Result<(), PanelError> {
    match change {
        CueChange::Mode(m) => def.mode = *m,
        CueChange::NoRepeatLast(n) => def.no_repeat_last = *n,
        CueChange::Pitch(lo, hi) => {
            let (lo, hi) = (finite(*lo, "pitch_min")?, finite(*hi, "pitch_max")?);
            if lo <= 0.0 || hi < lo {
                return Err(err(
                    "bad_value",
                    format!("pitch must satisfy 0 < pitch_min <= pitch_max, got {lo}..{hi}"),
                ));
            }
            def.pitch_min = lo;
            def.pitch_max = hi;
        }
        CueChange::VolumeRange(lo, hi) => {
            let (lo, hi) = (finite(*lo, "volume_db_min")?, finite(*hi, "volume_db_max")?);
            if hi < lo {
                return Err(err(
                    "bad_value",
                    format!("volume range must satisfy min <= max, got {lo}..{hi}"),
                ));
            }
            def.volume_db_min = lo;
            def.volume_db_max = hi;
        }
        CueChange::MaxConcurrent(n) => def.max_concurrent = *n,
        CueChange::Steal(s) => def.steal = *s,
        CueChange::CooldownMs(ms) => def.cooldown = Duration::from_millis(*ms),
        CueChange::BaseVolume(db) => def.volume_db = finite(*db, "volume_db")?,
        CueChange::Bus(b) => def.bus.clone_from(b),
        CueChange::Filter {
            cutoff_hz,
            offset_octaves,
        } => {
            let offset = finite(*offset_octaves, "filter_offset_octaves")?;
            match cutoff_hz {
                Some(hz) => {
                    let hz = finite(*hz, "filter_cutoff_hz")?;
                    if hz <= 0.0 {
                        return Err(err("bad_value", "`filter_cutoff_hz` must be positive"));
                    }
                    def.filter_cutoff_hz = Some(hz);
                }
                None => def.filter_cutoff_hz = None,
            }
            def.filter_offset_octaves = offset;
        }
        CueChange::Caption(c) => {
            if let Some(caption) = c
                && caption.key.trim().is_empty()
            {
                return Err(err("bad_value", "a caption key must not be empty"));
            }
            def.caption.clone_from(c);
        }
        CueChange::AddSound(path, weight) => {
            let weight = finite(*weight, "weight")?;
            if weight < 0.0 {
                return Err(err("bad_value", "`weight` must not be negative"));
            }
            if path.trim().is_empty() {
                return Err(err("bad_value", "`sound` must not be empty"));
            }
            if def.sounds.len() >= MAX_SOUNDS {
                return Err(err(
                    "too_many_sounds",
                    format!("a cue holds at most {MAX_SOUNDS} variations"),
                ));
            }
            def.sounds.push(regia_audio::CueSoundDef {
                sound: path.clone(),
                weight,
            });
        }
        CueChange::RemoveSound(i) => {
            if *i >= def.sounds.len() {
                return Err(err(
                    "bad_index",
                    format!("no variation {i} (the cue has {})", def.sounds.len()),
                ));
            }
            def.sounds.remove(*i);
        }
        CueChange::Weight(i, w) => {
            let w = finite(*w, "weight")?;
            if w < 0.0 {
                return Err(err("bad_value", "`weight` must not be negative"));
            }
            let Some(s) = def.sounds.get_mut(*i) else {
                return Err(err(
                    "bad_index",
                    format!("no variation {i} (the cue has {})", def.sounds.len()),
                ));
            };
            s.weight = w;
        }
    }
    Ok(())
}

/// The open cue.
#[derive(Clone, Debug)]
pub struct CueDoc {
    /// Project-relative path of the `.cue.ron`.
    pub path: String,
    /// The file's text when it was opened or last saved (byte-exact).
    pub original: String,
    /// The definition as that text parses.
    pub saved: SoundCueDef,
    /// The definition being edited.
    pub def: SoundCueDef,
}

impl CueDoc {
    /// Whether the edited definition differs from the file.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.def != self.saved
    }
}

/// One audition: which variation the preview picked.
#[derive(Clone, Debug, PartialEq)]
pub struct Audition {
    /// Index of the picked variation.
    pub index: usize,
    /// Its sound path.
    pub sound: String,
    /// The volume the pick plays at: the base plus a draw from the range, dB.
    pub volume_db: f32,
    /// The pitch ratio drawn from the range.
    pub pitch: f32,
    /// Whether the editor's preview output played it (false when the file could not be decoded).
    pub played: bool,
}

/// The cue editor's state (a resource of the edit world).
#[derive(Resource, Debug, Default)]
pub struct CueEditorPanel {
    doc: Option<CueDoc>,
    audition_count: u64,
    last_audition: Option<Audition>,
    last_error: Option<String>,
    /// UI scratch: the path box and the add-variation box.
    path_input: String,
    add_input: String,
    /// UI scratch: the caption row's key, kind, priority and planned length.
    caption_key: String,
    caption_kind: CueCaptionKind,
    caption_priority: u8,
    caption_planned_ms: u32,
}

impl CueEditorPanel {
    /// The open cue.
    #[must_use]
    pub fn doc(&self) -> Option<&CueDoc> {
        self.doc.as_ref()
    }

    /// The last audition.
    #[must_use]
    pub fn last_audition(&self) -> Option<&Audition> {
        self.last_audition.as_ref()
    }
}

/// SplitMix64 step, for the audition draws (pure: the same count draws the same variation).
fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A draw in `[0, 1)` for stream `n` of `seed`.
fn unit(seed: u64, n: u64) -> f32 {
    ((mix64(seed ^ mix64(n)) >> 40) as f32) / (1u64 << 24) as f32
}

/// Picks the variation the cue would play for audition number `count`: sequential cues walk the list;
/// random and shuffle cues draw by weight. `None` when no variation can be picked.
#[must_use]
pub fn pick_variation(def: &SoundCueDef, count: u64) -> Option<usize> {
    if def.sounds.is_empty() {
        return None;
    }
    if def.mode == CuePlayMode::Sequential {
        return Some((count % def.sounds.len() as u64) as usize);
    }
    let total: f32 = def.sounds.iter().map(|s| s.weight.max(0.0)).sum();
    if total <= 0.0 {
        return None;
    }
    let mut x = unit(0xA5D1_7000, count) * total;
    for (i, s) in def.sounds.iter().enumerate() {
        x -= s.weight.max(0.0);
        if x < 0.0 {
            return Some(i);
        }
    }
    Some(def.sounds.len() - 1)
}

/// One reversible cue change (one undo step).
pub struct CueEdit {
    before: SoundCueDef,
    after: SoundCueDef,
    key: Option<String>,
    label: &'static str,
}

impl CueEdit {
    fn set(world: &mut World, def: &SoundCueDef) {
        let mut panel = world.remove_resource::<CueEditorPanel>().unwrap_or_default();
        if let Some(doc) = panel.doc.as_mut() {
            doc.def = def.clone();
        }
        world.insert_resource(panel);
    }
}

impl EditorCommand for CueEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<CueEdit>() else {
            return false;
        };
        if self.key.is_some() && self.key == next.key {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        format!("Cue: {}", self.label)
    }
}

/// Opens the cue at the project-relative `path`.
///
/// # Errors
///
/// `cue_editor_no_project`, `cue_editor_bad_path`, `cue_editor_io_failed` or `cue_editor_parse_failed`.
pub fn open_cue(world: &mut World, path: &str) -> Result<(), PanelError> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| err("no_project", "no project is open; open one first"))?;
    let full = confined(&root, path, &[".cue.ron"], "cue_editor")?;
    let text =
        std::fs::read_to_string(&full).map_err(|e| err("io_failed", format!("{}: {e}", full.display())))?;
    let def = SoundCueDef::from_ron(&text).map_err(|e| err("parse_failed", format!("{path}: {e}")))?;
    let mut panel = world.remove_resource::<CueEditorPanel>().unwrap_or_default();
    panel.path_input = path.to_owned();
    // The caption row opens with the file's caption, so editing it starts from its values.
    if let Some(caption) = def.caption.as_ref() {
        panel.caption_key = caption.key.clone();
        panel.caption_kind = caption.kind;
        panel.caption_priority = caption.priority;
        panel.caption_planned_ms = caption.planned_ms.unwrap_or(0);
    }
    panel.doc = Some(CueDoc {
        path: path.to_owned(),
        original: text,
        saved: def.clone(),
        def,
    });
    panel.last_audition = None;
    panel.last_error = None;
    world.insert_resource(panel);
    Ok(())
}

/// Applies `change` to the open cue as one undo step. `Ok(false)` when it changed nothing.
///
/// # Errors
///
/// `cue_editor_no_cue` when none is open, else [`apply_change`]'s errors.
pub fn edit_cue(world: &mut World, change: &CueChange) -> Result<bool, PanelError> {
    let before = world
        .get_resource::<CueEditorPanel>()
        .and_then(|p| p.doc.as_ref())
        .map(|d| d.def.clone())
        .ok_or_else(|| err("no_cue", "no cue is open; pass `path`"))?;
    let mut after = before.clone();
    apply_change(&mut after, change)?;
    if after == before {
        return Ok(false);
    }
    let mut command = CueEdit {
        before,
        after,
        key: change.key(),
        label: change.label(),
    };
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    Ok(true)
}

/// Saves the open cue. An unedited cue is left alone (the file keeps its bytes); an edited one is written
/// with `SoundCueDef::to_ron`, atomically. Returns the bytes written (0 when nothing was written).
///
/// # Errors
///
/// `cue_editor_no_cue`, `cue_editor_no_project`, `cue_editor_serialize_failed` or `cue_editor_io_failed`.
pub fn save_cue(world: &mut World) -> Result<usize, PanelError> {
    let root =
        crate::mesh_source::project_root(world).ok_or_else(|| err("no_project", "no project is open"))?;
    let mut panel = world.remove_resource::<CueEditorPanel>().unwrap_or_default();
    let result = (|| -> Result<_, PanelError> {
        let doc = panel
            .doc
            .as_mut()
            .ok_or_else(|| err("no_cue", "no cue is open"))?;
        if !doc.dirty() {
            return Ok(0);
        }
        let text = doc
            .def
            .to_ron()
            .map_err(|e| err("serialize_failed", e.to_string()))?;
        let full = confined(&root, &doc.path, &[".cue.ron"], "cue_editor")?;
        write_atomic(&full, &text, "cue_editor")?;
        let written = text.len();
        doc.original = text;
        doc.saved = doc.def.clone();
        Ok(written)
    })();
    world.insert_resource(panel);
    result
}

/// Plays one audition of the open cue: picks a variation and draws its volume and pitch. With `play`, the
/// picked file (a `.wav` in the project) is also played on the editor's preview output; the pitch and volume
/// draws are reported, not applied to the preview.
///
/// # Errors
///
/// `cue_editor_no_cue`, or `cue_editor_empty` when no variation can be picked (none, or all weights zero).
pub fn audition(world: &mut World, play: bool) -> Result<Audition, PanelError> {
    let root = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<CueEditorPanel>().unwrap_or_default();
    let result = (|| -> Result<_, PanelError> {
        let doc = panel
            .doc
            .as_ref()
            .ok_or_else(|| err("no_cue", "no cue is open"))?;
        let count = panel.audition_count;
        let index = pick_variation(&doc.def, count)
            .ok_or_else(|| err("empty", "the cue has no variation with a positive weight"))?;
        let def = &doc.def;
        let v = unit(0x00C0_FFEE, count);
        let p = unit(0x00BE_EF00, count);
        let volume_db = def.volume_db + def.volume_db_min + (def.volume_db_max - def.volume_db_min) * v;
        let pitch = def.pitch_min + (def.pitch_max - def.pitch_min) * p;
        let sound = def.sounds[index].sound.clone();
        let played = play && root.as_deref().is_some_and(|r| play_file(r, &sound));
        Ok(Audition {
            index,
            sound,
            volume_db,
            pitch,
            played,
        })
    })();
    if let Ok(a) = &result {
        panel.audition_count += 1;
        panel.last_audition = Some(a.clone());
    }
    world.insert_resource(panel);
    result
}

/// Reads and decodes the project sound `sound` (a plain relative path under `root`) for the preview:
/// WAV, Ogg Vorbis, FLAC or MP3, through `asset_viewer::decode_sound`. `None` when the path is not
/// plain or the file cannot be read or decoded.
fn decode_file(root: &Path, sound: &str) -> Option<crate::asset_viewer::DecodedSound> {
    let rel = Path::new(sound);
    let plain = rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !plain {
        return None;
    }
    let full: PathBuf = root.join(rel);
    let bytes = std::fs::read(&full).ok()?;
    crate::asset_viewer::decode_sound(&bytes, sound).ok()
}

/// Plays a project sound on the editor's preview output; false when it cannot be read or decoded.
fn play_file(root: &Path, sound: &str) -> bool {
    let Some(decoded) = decode_file(root, sound) else {
        return false;
    };
    crate::asset_viewer::play_sound(&decoded).unwrap_or(false)
}

// --------------------------------------------------------------------------------------- panel

fn mode_name(m: CuePlayMode) -> &'static str {
    match m {
        CuePlayMode::Random => "random",
        CuePlayMode::Shuffle => "shuffle",
        CuePlayMode::Sequential => "sequential",
    }
}

fn steal_name(s: StealPolicy) -> &'static str {
    match s {
        StealPolicy::Oldest => "oldest",
        StealPolicy::Quietest => "quietest",
        StealPolicy::None => "none",
    }
}

/// Draws the panel.
pub fn cue_editor_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<CueEditorPanel>().unwrap_or_default();
    let mut open: Option<String> = None;
    let mut change: Option<CueChange> = None;
    let mut do_save = false;
    let mut do_audition = false;
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut panel.path_input).hint_text("audio/footstep.cue.ron"));
        if ui.button("Open").clicked() {
            open = Some(panel.path_input.clone());
        }
    });
    if let Some(doc) = panel.doc.clone() {
        let def = &doc.def;
        ui.horizontal(|ui| {
            ui.strong(&def.name);
            ui.weak(format!(
                "{}{}",
                doc.path,
                if doc.dirty() { " (edited)" } else { "" }
            ));
            if ui.add_enabled(doc.dirty(), egui::Button::new("Save")).clicked() {
                do_save = true;
            }
            if ui.button("Audition").clicked() {
                do_audition = true;
            }
        });
        ui.separator();
        ui.label("Variations");
        egui::Grid::new("cue-sounds").num_columns(3).show(ui, |ui| {
            for (i, s) in def.sounds.iter().enumerate() {
                ui.label(&s.sound);
                let mut w = s.weight;
                if ui
                    .add(egui::DragValue::new(&mut w).speed(0.05).range(0.0..=1000.0))
                    .changed()
                {
                    change = Some(CueChange::Weight(i, w));
                }
                if ui.small_button("Remove").clicked() {
                    change = Some(CueChange::RemoveSound(i));
                }
                ui.end_row();
            }
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut panel.add_input).hint_text("audio/step_03.wav"));
            if ui.button("Add variation").clicked() && !panel.add_input.trim().is_empty() {
                change = Some(CueChange::AddSound(panel.add_input.trim().to_owned(), 1.0));
                panel.add_input.clear();
            }
        });
        ui.separator();
        let mut mode = def.mode;
        egui::ComboBox::from_id_salt("cue-mode")
            .selected_text(format!("mode: {}", mode_name(mode)))
            .show_ui(ui, |ui| {
                for m in [CuePlayMode::Random, CuePlayMode::Shuffle, CuePlayMode::Sequential] {
                    ui.selectable_value(&mut mode, m, mode_name(m));
                }
            });
        if mode != def.mode {
            change = Some(CueChange::Mode(mode));
        }
        let mut steal = def.steal;
        egui::ComboBox::from_id_salt("cue-steal")
            .selected_text(format!("steal: {}", steal_name(steal)))
            .show_ui(ui, |ui| {
                for s in [StealPolicy::Oldest, StealPolicy::Quietest, StealPolicy::None] {
                    ui.selectable_value(&mut steal, s, steal_name(s));
                }
            });
        if steal != def.steal {
            change = Some(CueChange::Steal(steal));
        }
        ui.horizontal(|ui| {
            ui.label("volume dB");
            let mut v = def.volume_db;
            if ui.add(egui::DragValue::new(&mut v).speed(0.1)).changed() {
                change = Some(CueChange::BaseVolume(v));
            }
            ui.label("range");
            let (mut lo, mut hi) = (def.volume_db_min, def.volume_db_max);
            let a = ui.add(egui::DragValue::new(&mut lo).speed(0.1)).changed();
            let b = ui.add(egui::DragValue::new(&mut hi).speed(0.1)).changed();
            if a || b {
                change = Some(CueChange::VolumeRange(lo, hi));
            }
        });
        ui.horizontal(|ui| {
            ui.label("pitch");
            let (mut lo, mut hi) = (def.pitch_min, def.pitch_max);
            let a = ui
                .add(egui::DragValue::new(&mut lo).speed(0.01).range(0.05..=8.0))
                .changed();
            let b = ui
                .add(egui::DragValue::new(&mut hi).speed(0.01).range(0.05..=8.0))
                .changed();
            if a || b {
                change = Some(CueChange::Pitch(lo, hi));
            }
            ui.label("max instances");
            let mut n = def.max_concurrent.unwrap_or(0);
            if ui.add(egui::DragValue::new(&mut n).range(0..=256)).changed() {
                change = Some(CueChange::MaxConcurrent((n > 0).then_some(n)));
            }
        });
        // The filter row: the base low-pass cutoff (off = no filter) and the octave offset.
        ui.horizontal(|ui| {
            let mut on = def.filter_cutoff_hz.is_some();
            if ui.checkbox(&mut on, "low-pass").changed() {
                change = Some(CueChange::Filter {
                    cutoff_hz: on.then_some(def.filter_cutoff_hz.unwrap_or(20_000.0)),
                    offset_octaves: def.filter_offset_octaves,
                });
            }
            if let Some(hz) = def.filter_cutoff_hz {
                let mut value = hz;
                if ui
                    .add(
                        egui::DragValue::new(&mut value)
                            .speed(10.0)
                            .range(20.0..=20_000.0)
                            .suffix(" Hz"),
                    )
                    .changed()
                {
                    change = Some(CueChange::Filter {
                        cutoff_hz: Some(value),
                        offset_octaves: def.filter_offset_octaves,
                    });
                }
            }
            let mut octaves = def.filter_offset_octaves;
            if ui
                .add(
                    egui::DragValue::new(&mut octaves)
                        .speed(0.01)
                        .range(-8.0..=8.0)
                        .suffix(" oct"),
                )
                .changed()
            {
                change = Some(CueChange::Filter {
                    cutoff_hz: def.filter_cutoff_hz,
                    offset_octaves: octaves,
                });
            }
        });
        // The caption row: the Fluent key a play sends, its kind, priority and planned length.
        ui.horizontal(|ui| {
            let mut on = def.caption.is_some();
            if ui.checkbox(&mut on, "caption").changed() {
                change = Some(CueChange::Caption(on.then(|| CueCaptionDef {
                    key: panel.caption_key.clone(),
                    kind: panel.caption_kind,
                    priority: panel.caption_priority,
                    planned_ms: (panel.caption_planned_ms > 0).then_some(panel.caption_planned_ms),
                })));
            }
            if def.caption.is_some() || on {
                ui.add(
                    egui::TextEdit::singleline(&mut panel.caption_key)
                        .hint_text("caption key")
                        .desired_width(140.0),
                );
                egui::ComboBox::from_id_salt("cue-caption-kind")
                    .selected_text(caption_kind_name(&panel.caption_kind))
                    .show_ui(ui, |ui| {
                        for (kind, name) in [
                            (CueCaptionKind::Sound, "sound"),
                            (CueCaptionKind::Speech, "speech"),
                            (CueCaptionKind::Narration, "narration"),
                        ] {
                            ui.selectable_label(panel.caption_kind == kind, name)
                                .clicked()
                                .then(|| panel.caption_kind = kind);
                        }
                    });
                let mut priority = panel.caption_priority;
                if ui
                    .add(egui::DragValue::new(&mut priority).range(0..=255).prefix("p "))
                    .changed()
                {
                    panel.caption_priority = priority;
                }
                let mut planned = panel.caption_planned_ms;
                if ui
                    .add(
                        egui::DragValue::new(&mut planned)
                            .range(0..=600_000)
                            .suffix(" ms"),
                    )
                    .changed()
                {
                    panel.caption_planned_ms = planned;
                }
                if ui.button("Set caption").clicked() {
                    let key = panel.caption_key.trim().to_owned();
                    if key.is_empty() {
                        panel.last_error = Some(err("bad_value", "`caption.key` must not be empty").message);
                    } else {
                        change = Some(CueChange::Caption(Some(CueCaptionDef {
                            key,
                            kind: panel.caption_kind,
                            priority: panel.caption_priority,
                            planned_ms: (panel.caption_planned_ms > 0).then_some(panel.caption_planned_ms),
                        })));
                    }
                }
            }
        });
        if let Some(a) = &panel.last_audition {
            ui.weak(format!(
                "audition: {} (variation {}), {:+.1} dB, pitch {:.2}{}",
                a.sound,
                a.index,
                a.volume_db,
                a.pitch,
                if a.played { "" } else { " (not played)" }
            ));
        }
    } else {
        ui.weak("Open a .cue.ron from the project to edit its variations.");
    }
    if let Some(e) = &panel.last_error {
        ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e);
    }
    world.insert_resource(panel);

    let mut result: Result<(), PanelError> = Ok(());
    if let Some(path) = open {
        result = open_cue(world, &path);
    }
    if let Some(change) = change {
        result = result.and_then(|()| edit_cue(world, &change).map(|_| ()));
    }
    if do_save {
        result = result.and_then(|()| save_cue(world).map(|_| ()));
    }
    if do_audition {
        result = result.and_then(|()| audition(world, true).map(|_| ()));
    }
    let mut panel = world.remove_resource::<CueEditorPanel>().unwrap_or_default();
    panel.last_error = result.err().map(|e| e.message);
    world.insert_resource(panel);
}

// --------------------------------------------------------------------------------------- agent surface

fn doc_json(world: &World) -> Value {
    let Some(doc) = world
        .get_resource::<CueEditorPanel>()
        .and_then(|p| p.doc.as_ref())
    else {
        return json!({ "open": false });
    };
    let def = &doc.def;
    json!({
        "open": true,
        "path": doc.path,
        "dirty": doc.dirty(),
        "name": def.name,
        "bus": def.bus,
        "mode": mode_name(def.mode),
        "steal": steal_name(def.steal),
        "no_repeat_last": def.no_repeat_last,
        "pitch": [def.pitch_min, def.pitch_max],
        "volume_db": def.volume_db,
        "volume_range_db": [def.volume_db_min, def.volume_db_max],
        "max_concurrent": def.max_concurrent,
        "cooldown_ms": def.cooldown.as_millis() as u64,
        "filter_cutoff_hz": def.filter_cutoff_hz,
        "filter_offset_octaves": def.filter_offset_octaves,
        "caption": def.caption.as_ref().map(|c| json!({
            "key": c.key,
            "kind": caption_kind_name(&c.kind),
            "priority": c.priority,
            "planned_ms": c.planned_ms,
        })),
        "sounds": def.sounds.iter().map(|s| json!({ "sound": s.sound, "weight": s.weight })).collect::<Vec<_>>(),
    })
}

fn num(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    crate::data_panels::opt_f32(params, key)
}

fn count(params: &Value, key: &str) -> Result<Option<u64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a non-negative integer"))),
    }
}

fn parse_mode(s: &str) -> Result<CuePlayMode, RpcError> {
    match s {
        "random" => Ok(CuePlayMode::Random),
        "shuffle" => Ok(CuePlayMode::Shuffle),
        "sequential" => Ok(CuePlayMode::Sequential),
        other => Err(RpcError::invalid_params(format!(
            "`mode` must be random, shuffle or sequential, got `{other}`"
        ))),
    }
}

fn parse_steal(s: &str) -> Result<StealPolicy, RpcError> {
    match s {
        "oldest" => Ok(StealPolicy::Oldest),
        "quietest" => Ok(StealPolicy::Quietest),
        "none" => Ok(StealPolicy::None),
        other => Err(RpcError::invalid_params(format!(
            "`steal` must be oldest, quietest or none, got `{other}`"
        ))),
    }
}

/// The changes a `set` call asks for, in a fixed order.
fn changes_of(world: &World, params: &Value) -> Result<Vec<CueChange>, RpcError> {
    let current = world
        .get_resource::<CueEditorPanel>()
        .and_then(|p| p.doc.as_ref())
        .map(|d| d.def.clone());
    let mut out = Vec::new();
    if let Some(m) = params.get("mode").and_then(Value::as_str) {
        out.push(CueChange::Mode(parse_mode(m)?));
    }
    if let Some(s) = params.get("steal").and_then(Value::as_str) {
        out.push(CueChange::Steal(parse_steal(s)?));
    }
    if let Some(n) = count(params, "no_repeat_last")? {
        out.push(CueChange::NoRepeatLast(n as usize));
    }
    let (pmin, pmax) = (num(params, "pitch_min")?, num(params, "pitch_max")?);
    if pmin.is_some() || pmax.is_some() {
        let c = current.as_ref();
        out.push(CueChange::Pitch(
            pmin.or(c.map(|d| d.pitch_min)).unwrap_or(1.0),
            pmax.or(c.map(|d| d.pitch_max)).unwrap_or(1.0),
        ));
    }
    let (vmin, vmax) = (num(params, "volume_db_min")?, num(params, "volume_db_max")?);
    if vmin.is_some() || vmax.is_some() {
        let c = current.as_ref();
        out.push(CueChange::VolumeRange(
            vmin.or(c.map(|d| d.volume_db_min)).unwrap_or(0.0),
            vmax.or(c.map(|d| d.volume_db_max)).unwrap_or(0.0),
        ));
    }
    if let Some(v) = num(params, "volume_db")? {
        out.push(CueChange::BaseVolume(v));
    }
    match params.get("max_concurrent") {
        None => {}
        Some(Value::Null) => out.push(CueChange::MaxConcurrent(None)),
        Some(v) => out.push(CueChange::MaxConcurrent(Some(v.as_u64().ok_or_else(|| {
            RpcError::invalid_params("`max_concurrent` must be a non-negative integer or null")
        })? as usize))),
    }
    if let Some(ms) = count(params, "cooldown_ms")? {
        out.push(CueChange::CooldownMs(ms));
    }
    match params.get("bus") {
        None => {}
        Some(Value::Null) => out.push(CueChange::Bus(None)),
        Some(Value::String(b)) => out.push(CueChange::Bus(Some(b.clone()))),
        Some(_) => return Err(RpcError::invalid_params("`bus` must be a string or null")),
    }
    let (cutoff, offset) = (
        params.get("filter_cutoff_hz"),
        params.get("filter_offset_octaves"),
    );
    if cutoff.is_some() || offset.is_some() {
        // One change carries both filter fields; the one left out keeps its current value.
        let cutoff = match cutoff {
            None => current.as_ref().and_then(|d| d.filter_cutoff_hz),
            Some(Value::Null) => None,
            Some(v) => Some(
                v.as_f64()
                    .filter(|v| v.is_finite())
                    .map(|v| v as f32)
                    .ok_or_else(|| {
                        RpcError::invalid_params("`filter_cutoff_hz` must be a finite number or null")
                    })?,
            ),
        };
        let offset = match offset {
            None => current.as_ref().map_or(0.0, |d| d.filter_offset_octaves),
            Some(v) => v
                .as_f64()
                .filter(|v| v.is_finite())
                .map(|v| v as f32)
                .ok_or_else(|| RpcError::invalid_params("`filter_offset_octaves` must be a finite number"))?,
        };
        out.push(CueChange::Filter {
            cutoff_hz: cutoff,
            offset_octaves: offset,
        });
    }
    match params.get("caption") {
        None => {}
        Some(Value::Null) => out.push(CueChange::Caption(None)),
        Some(Value::Object(_)) => {
            let caption = caption_of(params).map_err(|e| e.rpc(HINT))?;
            out.push(CueChange::Caption(Some(caption)));
        }
        Some(_) => return Err(RpcError::invalid_params("`caption` must be an object or null")),
    }
    Ok(out)
}

fn cue_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(path) = params.get("path").and_then(Value::as_str) {
        open_cue(world, path).map_err(|e| e.rpc(HINT))?;
    }
    let op = params.get("op").and_then(Value::as_str).unwrap_or("get");
    let mut extra = json!({});
    match op {
        "get" => {
            if world
                .get_resource::<CueEditorPanel>()
                .and_then(|p| p.doc.as_ref())
                .is_none()
            {
                return Err(err("no_cue", "no cue is open; pass `path`").rpc(HINT));
            }
        }
        "set" => {
            let changes = changes_of(world, params)?;
            if changes.is_empty() {
                return Err(RpcError::invalid_params(
                    "`set` needs at least one of mode, steal, no_repeat_last, pitch_min, pitch_max, \
                     volume_db, volume_db_min, volume_db_max, max_concurrent, cooldown_ms, bus, \
                     filter_cutoff_hz, filter_offset_octaves, caption",
                ));
            }
            // All or nothing: validate every change on a copy first.
            let mut probe = world
                .get_resource::<CueEditorPanel>()
                .and_then(|p| p.doc.as_ref())
                .map(|d| d.def.clone())
                .ok_or_else(|| err("no_cue", "no cue is open; pass `path`").rpc(HINT))?;
            for c in &changes {
                apply_change(&mut probe, c).map_err(|e| e.rpc(HINT))?;
            }
            for c in &changes {
                edit_cue(world, c).map_err(|e| e.rpc(HINT))?;
            }
        }
        "add_sound" => {
            let sound = params
                .get("sound")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("`sound` (a path) is required"))?;
            let weight = num(params, "weight")?.unwrap_or(1.0);
            edit_cue(world, &CueChange::AddSound(sound.to_owned(), weight)).map_err(|e| e.rpc(HINT))?;
        }
        "remove_sound" | "set_weight" => {
            let index = count(params, "index")?
                .ok_or_else(|| RpcError::invalid_params("`index` is required"))?
                as usize;
            let change = if op == "remove_sound" {
                CueChange::RemoveSound(index)
            } else {
                let w =
                    num(params, "weight")?.ok_or_else(|| RpcError::invalid_params("`weight` is required"))?;
                CueChange::Weight(index, w)
            };
            edit_cue(world, &change).map_err(|e| e.rpc(HINT))?;
        }
        "save" => {
            let written = save_cue(world).map_err(|e| e.rpc(HINT))?;
            extra = json!({ "written": written });
        }
        "audition" => {
            let play = params.get("play").and_then(Value::as_bool).unwrap_or(false);
            let a = audition(world, play).map_err(|e| e.rpc(HINT))?;
            extra = json!({ "audition": {
                "index": a.index, "sound": a.sound, "volume_db": a.volume_db, "pitch": a.pitch, "played": a.played,
            } });
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "`op` must be get, set, add_sound, remove_sound, set_weight, save or audition, got `{other}`"
            )));
        }
    }
    let mut out = doc_json(world);
    if let (Some(map), Some(more)) = (out.as_object_mut(), extra.as_object()) {
        map.extend(more.clone());
    }
    out["undo_depth"] = json!(world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth));
    Ok(out)
}

const EDIT_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "path",
        kind: "string",
        required: false,
        description: "Project-relative .cue.ron to open first (a different path replaces the open cue).",
    },
    ParamSchema {
        name: "op",
        kind: "string",
        required: false,
        description: "get (default), set, add_sound, remove_sound, set_weight, save or audition.",
    },
    ParamSchema {
        name: "mode",
        kind: "string",
        required: false,
        description: "set: random, shuffle or sequential.",
    },
    ParamSchema {
        name: "steal",
        kind: "string",
        required: false,
        description: "set: oldest, quietest or none (what happens at max_concurrent).",
    },
    ParamSchema {
        name: "pitch_min",
        kind: "number",
        required: false,
        description: "set: lowest random pitch ratio (> 0).",
    },
    ParamSchema {
        name: "pitch_max",
        kind: "number",
        required: false,
        description: "set: highest random pitch ratio (>= pitch_min).",
    },
    ParamSchema {
        name: "volume_db",
        kind: "number",
        required: false,
        description: "set: base volume, dB.",
    },
    ParamSchema {
        name: "volume_db_min",
        kind: "number",
        required: false,
        description: "set: lowest random volume offset, dB.",
    },
    ParamSchema {
        name: "volume_db_max",
        kind: "number",
        required: false,
        description: "set: highest random volume offset, dB (>= volume_db_min).",
    },
    ParamSchema {
        name: "max_concurrent",
        kind: "number",
        required: false,
        description: "set: instance limit; null removes it.",
    },
    ParamSchema {
        name: "no_repeat_last",
        kind: "number",
        required: false,
        description: "set: never repeat the last N picks.",
    },
    ParamSchema {
        name: "cooldown_ms",
        kind: "number",
        required: false,
        description: "set: minimum time between plays, milliseconds.",
    },
    ParamSchema {
        name: "bus",
        kind: "string",
        required: false,
        description: "set: the bus every pick plays on; null = Master.",
    },
    ParamSchema {
        name: "filter_cutoff_hz",
        kind: "number",
        required: false,
        description: "set: base low-pass cutoff in Hz; null removes the filter.",
    },
    ParamSchema {
        name: "filter_offset_octaves",
        kind: "number",
        required: false,
        description: "set: octaves added to the low-pass cutoff.",
    },
    ParamSchema {
        name: "caption",
        kind: "any",
        required: false,
        description: "set: { key, kind? (sound | speech | narration), priority?, planned_ms? } or \
            null; the caption a play sends.",
    },
    ParamSchema {
        name: "sound",
        kind: "string",
        required: false,
        description: "add_sound: the sound file path.",
    },
    ParamSchema {
        name: "index",
        kind: "number",
        required: false,
        description: "remove_sound, set_weight: the variation index.",
    },
    ParamSchema {
        name: "weight",
        kind: "number",
        required: false,
        description: "add_sound (default 1), set_weight: the pick weight (>= 0).",
    },
    ParamSchema {
        name: "play",
        kind: "boolean",
        required: false,
        description: "audition: also play the picked file on the editor's preview output (default false).",
    },
];

/// The cue editor's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.cue_edit",
            tool: "cue_edit",
            description: "The sound cue editor (spec 16 § 5): open a .cue.ron and read or edit its variations, \
                weights, pick mode, steal policy, volume and pitch ranges, low-pass filter and caption \
                (each change one undo step), save it (an unedited cue keeps its bytes), or audition a pick.",
            params: EDIT_PARAMS,
            result: "{ open, path, dirty, name, bus, mode, steal, no_repeat_last, pitch: [min, max], volume_db, \
                volume_range_db: [min, max], max_concurrent, cooldown_ms, filter_cutoff_hz, \
                filter_offset_octaves, caption: { key, kind, priority, planned_ms }, sounds: [{ sound, \
                weight }], undo_depth, written?, audition?: { index, sound, volume_db, pitch, played } }",
            mutating: true,
            permission: Permission::Assets,
        },
        cue_edit as Handler,
    )]
}

pub(crate) mod autosave;

#[cfg(test)]
mod tests;
