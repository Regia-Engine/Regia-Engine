// SPDX-License-Identifier: MPL-2.0
//! The Montage timeline (`docs/specs/34-montage-extensions.md` M10, ADR-2031): a `.montage.ron` (or a child
//! `.montagevar.ron`) as lanes — one per slot track with its clip segments, then sections, notifies and
//! warp windows — with a playhead and a preview of one section chain or the whole timeline order. Agent
//! surface: `editor.montage_panel_*` (`book/agents/editor-data-panels.md`).
//!
//! The panel is thin over the `anim.*` montage methods: it reads with `anim.montage_describe` and writes with
//! `anim.montage_edit` (the same functions, so the same atomic writes, validation, follower moves and undo
//! stack). Dropping a clip from the Content Browser on a track lane adds a segment named by the file stem;
//! dragging a section marker moves the section; the selected item's fields edit below the lanes.
//!
//! Clip lengths come from the clip libraries of the open world's montage players and animators. A segment
//! that plays to the end of a clip nobody has loaded has no known length; the timeline draws it one second
//! long, hatched, and the describe answer carries `montage_clip_length_unknown`.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_animation::montage_edit::{FollowEdit, FollowModeEdit};
use regia_animation::{RotationKind, WarpAnchor, WarpMode};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::data_panels::{PanelError, from_rpc, opt_bool, opt_f32, str_param};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/editor-data-panels.md";

/// The length the timeline gives a segment whose length is unknown, seconds.
pub const UNKNOWN_LENGTH: f32 = 1.0;
/// Most steps a section-chain preview follows before it reports a loop.
pub const MAX_CHAIN: usize = 64;

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("montage_panel_{code}"), message)
}

/// The text of project-relative `rel` (`None` when it does not exist or cannot be read).
fn read_rel(root: &std::path::Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

/// A placed segment of the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedSegment {
    /// Track index.
    pub track: usize,
    /// Segment index in the track.
    pub segment: usize,
    /// Clip name.
    pub clip: String,
    /// Montage time it starts at.
    pub at: f32,
    /// Its length on the timeline (its describe length, else [`UNKNOWN_LENGTH`]).
    pub length: f32,
    /// Whether the length is known.
    pub known: bool,
    /// Clip-local start.
    pub start: f32,
    /// Clip-local end (`None` = the clip's end).
    pub end: Option<f32>,
    /// Rate.
    pub rate: f32,
    /// Loop count.
    pub loops: u32,
}

/// One step of a section-chain preview.
#[derive(Clone, Debug, PartialEq)]
pub struct ChainStep {
    /// The section.
    pub section: String,
    /// Its start, montage time.
    pub start: f32,
    /// Its end (the next section's start, else the timeline's length).
    pub end: f32,
}

/// What is selected on the timeline.
#[derive(Clone, Debug, PartialEq)]
pub enum Picked {
    /// A clip segment.
    Segment {
        /// Track index.
        track: usize,
        /// Segment index.
        segment: usize,
    },
    /// A section marker.
    Section(String),
    /// A notify.
    Notify(String),
    /// A warp window.
    Warp(usize),
}

impl Picked {
    fn json(&self) -> Value {
        match self {
            Self::Segment { track, segment } => {
                json!({ "kind": "segment", "track": track, "segment": segment })
            }
            Self::Section(name) => json!({ "kind": "section", "name": name }),
            Self::Notify(name) => json!({ "kind": "notify", "name": name }),
            Self::Warp(index) => json!({ "kind": "warp", "index": index }),
        }
    }
}

/// The Montage timeline's state (an edit-world resource).
#[derive(Resource)]
pub struct MontagePanel {
    /// The open asset, project-relative.
    pub asset: Option<String>,
    /// `anim.montage_describe`'s answer for it.
    pub described: Option<Value>,
    /// The playhead, montage time.
    pub playhead: f32,
    /// The section the chain preview starts at (`None` = the first).
    pub chain_from: Option<String>,
    /// Preview the whole timeline order instead of one chain.
    pub whole: bool,
    /// The selected item.
    pub picked: Option<Picked>,
    /// Pixels per second.
    pub zoom: f32,
    /// The last failed operation.
    pub error: Option<PanelError>,
    /// The last edit's report (`moved`, `unlinked`, `undo_depth`).
    pub last_edit: Option<Value>,
    /// The rig the pose preview samples on (an edit-world entity with a `SkinnedMesh`).
    pub rig: Option<Entity>,
    /// The last pose preview (refreshed when the playhead, the rig or the montage changes).
    pub pose: Option<Result<PosePreview, PanelError>>,
    pose_key: Option<(Option<u64>, u32, u64)>,
    reads: u64,
    path_input: String,
    add_clip: String,
    add_notify: String,
    open_requested: bool,
    use_selected_rig: bool,
    drag: Option<TimelineDrag>,
    /// The slot "Add to" uses (empty = the first track's slot; a slot no track plays starts a new track).
    add_slot: String,
    /// The warp-window form.
    warp: WarpForm,
    /// The blend form and the read it was loaded at.
    blend: Option<(u64, BlendForm)>,
    /// The follow-link form of the selected section or notify.
    follow: FollowForm,
    /// The section "Jump to" links the selected section to.
    link_to: String,
    /// The slot-table editor.
    slots: SlotTable,
    /// Slot-table requests collected while drawing.
    slot_requests: Vec<SlotRequest>,
    /// The panel's undo journal (sprint w11-forms-undo-autosave): montage edits (undone by the
    /// agent's `{ op: "undo" }`) and slot-table edits (undone by putting back the files they wrote),
    /// newest last.
    journal: Vec<UndoStep>,
}

/// Most steps the panel's undo journal keeps.
pub const JOURNAL_DEPTH: usize = 64;

/// One step of the panel's undo journal.
#[derive(Clone, Debug, PartialEq)]
enum UndoStep {
    /// An `anim.montage_edit` of the open asset: the agent's undo stack holds its text.
    Montage(String),
    /// An `anim.montage_slot_edit`: every file it wrote (project-relative) with its text before
    /// (`None`: the file did not exist, as a slot table that was the engine default).
    Slots(Vec<(String, Option<String>)>),
}

/// The warp-window form below the lanes (`add_warp`).
#[derive(Clone, Debug, PartialEq)]
pub struct WarpForm {
    /// Warp target name.
    pub target: String,
    /// Window start, montage time.
    pub start: f32,
    /// Window end, montage time.
    pub end: f32,
    /// Warp horizontal translation.
    pub translation: bool,
    /// Warp height.
    pub vertical: bool,
    /// Turn toward the target.
    pub rotation: bool,
    /// How the path maps onto the offset (spec 34 M2 `mode`).
    pub mode: WarpMode,
    /// Which facing the turn goes to (spec 34 M2 `rotation_kind`).
    pub rotation_kind: RotationKind,
    /// The fraction `(0, 1]` of the window the turn takes (spec 34 M2 `rotation_fraction`).
    pub rotation_fraction: f32,
    /// What lands on the target at the window's end (spec 34 M2 `anchor`).
    pub anchor: WarpAnchor,
}

impl Default for WarpForm {
    fn default() -> Self {
        Self {
            target: String::new(),
            start: 0.0,
            end: 0.5,
            translation: true,
            vertical: false,
            rotation: false,
            mode: WarpMode::Similarity,
            rotation_kind: RotationKind::Auto,
            rotation_fraction: 1.0,
            anchor: WarpAnchor::Root,
        }
    }
}

/// The `anim.montage_edit` name of a warp mode.
#[must_use]
pub fn warp_mode_name(mode: WarpMode) -> &'static str {
    match mode {
        WarpMode::Similarity => "similarity",
        WarpMode::PathScale => "path_scale",
    }
}

/// The `anim.montage_edit` name of a rotation kind.
#[must_use]
pub fn rotation_kind_name(kind: RotationKind) -> &'static str {
    match kind {
        RotationKind::Auto => "auto",
        RotationKind::MatchTarget => "match_target",
        RotationKind::FaceTarget => "face_target",
    }
}

impl WarpForm {
    /// The `anim.montage_edit` edit this form adds (every spec 34 M2 field: `mode`, `rotation_kind`,
    /// `rotation_fraction`, and `anchor_bone` or `anchor_offset` for a bone or offset anchor).
    #[must_use]
    pub fn edit(&self) -> Value {
        let mut edit = json!({
            "op": "add_warp",
            "target": self.target.trim(),
            "start": self.start,
            "end": self.end,
            "translation": self.translation,
            "vertical": self.vertical,
            "rotation": self.rotation,
            "mode": warp_mode_name(self.mode),
            "rotation_kind": rotation_kind_name(self.rotation_kind),
            "rotation_fraction": self.rotation_fraction,
        });
        match &self.anchor {
            WarpAnchor::Root => {}
            WarpAnchor::Bone(bone) => edit["anchor_bone"] = json!(bone.trim()),
            WarpAnchor::Offset(v) => edit["anchor_offset"] = json!(v),
        }
        edit
    }

    /// Whether Add warp can send the form: a target, and a bone name for a bone anchor.
    #[must_use]
    pub fn complete(&self) -> bool {
        !self.target.trim().is_empty()
            && !matches!(&self.anchor, WarpAnchor::Bone(bone) if bone.trim().is_empty())
    }
}

/// The blend form (`set_blend`), loaded from the describe answer's `blend`.
#[derive(Clone, Debug, PartialEq)]
pub struct BlendForm {
    /// Blend-in seconds.
    pub blend_in: f32,
    /// Blend-out seconds.
    pub blend_out: f32,
    /// Seconds before the end the automatic blend-out starts.
    pub blend_out_lead: f32,
    /// Blend out automatically at the end.
    pub auto_blend_out: bool,
}

impl BlendForm {
    /// The blend settings of a describe answer.
    #[must_use]
    pub fn from_described(described: &Value) -> Self {
        let b = &described["blend"];
        Self {
            blend_in: f(&b["blend_in"]).unwrap_or(0.0),
            blend_out: f(&b["blend_out"]).unwrap_or(0.0),
            blend_out_lead: f(&b["blend_out_lead"]).unwrap_or(0.0),
            auto_blend_out: b["auto_blend_out"].as_bool().unwrap_or(true),
        }
    }

    /// The `set_blend` edit carrying the fields that differ from `described` (`None` when none do).
    #[must_use]
    pub fn edit(&self, described: &Value) -> Option<Value> {
        let now = Self::from_described(described);
        let mut edit = serde_json::Map::new();
        edit.insert("op".into(), json!("set_blend"));
        if self.blend_in != now.blend_in {
            edit.insert("blend_in".into(), json!(self.blend_in));
        }
        if self.blend_out != now.blend_out {
            edit.insert("blend_out".into(), json!(self.blend_out));
        }
        if self.blend_out_lead != now.blend_out_lead {
            edit.insert("blend_out_lead".into(), json!(self.blend_out_lead));
        }
        if self.auto_blend_out != now.auto_blend_out {
            edit.insert("auto_blend_out".into(), json!(self.auto_blend_out));
        }
        (edit.len() > 1).then_some(Value::Object(edit))
    }
}

/// The follow-link form (`set_follow`): the segment a section or notify follows and how.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FollowForm {
    /// Track index.
    pub track: usize,
    /// Segment index on that track.
    pub segment: usize,
    /// How the item moves with the segment.
    pub mode: FollowModeEdit,
}

impl FollowForm {
    fn json(&self) -> Value {
        json!({ "track": self.track, "segment": self.segment, "mode": self.mode })
    }
}

/// The `set_follow` edit for the section or notify (`kind`) `name`: link it to `follow`, or clear its link.
#[must_use]
pub fn follow_edit(kind: &str, name: &str, follow: Option<&FollowForm>) -> Value {
    json!({ "op": "set_follow", "kind": kind, "name": name, "follow": follow.map(FollowForm::json) })
}

/// The `link_sections` edit for section `from`: jump to `to`, stop at its end (`stop`), or play on into the
/// next section (neither).
#[must_use]
pub fn link_edit(from: &str, to: Option<&str>, stop: bool) -> Value {
    match (to, stop) {
        (_, true) => json!({ "op": "link_sections", "from": from, "stop": true }),
        (Some(to), false) => json!({ "op": "link_sections", "from": from, "to": to }),
        (None, false) => json!({ "op": "link_sections", "from": from }),
    }
}

/// The `add_segment` edit appending `clip` to the track that plays `slot` (a new track when none does).
#[must_use]
pub fn add_segment_edit(slot: &str, clip: &str) -> Value {
    json!({ "op": "add_segment", "slot": slot, "clip": clip })
}

/// The slot-table editor's state: the `.slots.ron` it edits and `anim.montage_slots`' answer for it.
#[derive(Clone, Debug, Default)]
pub struct SlotTable {
    /// The table, project-relative (`.slots.ron`).
    pub path: Option<String>,
    /// `anim.montage_slots`' answer for it.
    pub table: Option<Value>,
    path_input: String,
    new_name: String,
    new_group: String,
    rename: std::collections::BTreeMap<String, String>,
    force: bool,
    /// Each slot row's `set` form, keyed by the slot's name (seeded from the table on load).
    set_forms: std::collections::BTreeMap<String, SlotSetForm>,
}

/// One slot row's `set` form: the group and the joint mask (`joints` empty = full body).
#[derive(Clone, Debug, Default, PartialEq)]
struct SlotSetForm {
    group: String,
    /// The mask's joints, comma-separated.
    joints: String,
    include_children: bool,
}

impl SlotSetForm {
    /// The form of a slot as `anim.montage_slots` lists it.
    fn of(slot: &Value) -> Self {
        let joints = slot["mask"]["joints"]
            .as_array()
            .map(|j| j.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        Self {
            group: slot["group"].as_str().unwrap_or("default").to_owned(),
            joints,
            include_children: slot["mask"]["include_children"].as_bool().unwrap_or(true),
        }
    }
}

/// A slot-table request the panel collects while drawing.
#[derive(Clone, Debug, PartialEq)]
enum SlotRequest {
    /// Read the table at this path.
    Load(String),
    /// Apply one `anim.montage_slot_edit` op.
    Edit { op: Value, force: bool },
}

/// The `anim.montage_slot_edit` op adding slot `name` in `group` (empty = `default`), full body.
#[must_use]
pub fn slot_add_op(name: &str, group: &str) -> Value {
    if group.trim().is_empty() {
        json!({ "op": "add", "name": name })
    } else {
        json!({ "op": "add", "name": name, "group": group.trim() })
    }
}

/// The `anim.montage_slot_edit` op renaming slot `from` to `to` (every referencing montage and graph too).
#[must_use]
pub fn slot_rename_op(from: &str, to: &str) -> Value {
    json!({ "op": "rename", "from": from, "to": to })
}

/// The `anim.montage_slot_edit` op deleting slot `name`.
#[must_use]
pub fn slot_delete_op(name: &str) -> Value {
    json!({ "op": "delete", "name": name })
}

/// The `anim.montage_slot_edit` op setting slot `name`'s group (empty = unchanged) and joint mask:
/// `joints` is comma-separated joint names (`include_children` masks their descendants too); no
/// joint names make the slot full body.
#[must_use]
pub fn slot_set_op(name: &str, group: &str, joints: &str, include_children: bool) -> Value {
    let mut op = json!({ "op": "set", "name": name });
    if !group.trim().is_empty() {
        op["group"] = json!(group.trim());
    }
    let joints: Vec<&str> = joints
        .split(',')
        .map(str::trim)
        .filter(|j| !j.is_empty())
        .collect();
    if joints.is_empty() {
        op["full_body"] = json!(true);
    } else {
        op["joints"] = json!(joints);
        op["include_children"] = json!(include_children);
    }
    op
}

/// What a drag on the lanes does.
#[derive(Clone, Debug, PartialEq)]
enum TimelineDrag {
    /// Moves the playhead (started on the ruler).
    Scrub,
    /// Moves this section's start (started on its marker).
    Section(String),
}

impl Default for MontagePanel {
    fn default() -> Self {
        Self {
            asset: None,
            described: None,
            playhead: 0.0,
            chain_from: None,
            whole: false,
            picked: None,
            zoom: 120.0,
            error: None,
            last_edit: None,
            rig: None,
            pose: None,
            pose_key: None,
            reads: 0,
            path_input: String::new(),
            add_clip: String::new(),
            add_notify: String::new(),
            open_requested: false,
            use_selected_rig: false,
            drag: None,
            add_slot: String::new(),
            warp: WarpForm::default(),
            blend: None,
            follow: FollowForm::default(),
            link_to: String::new(),
            slots: SlotTable::default(),
            slot_requests: Vec::new(),
            journal: Vec::new(),
        }
    }
}

fn f(v: &Value) -> Option<f32> {
    v.as_f64().map(|x| x as f32)
}

/// The segments of a describe answer placed on the timeline: back to back per track from 0, with
/// [`UNKNOWN_LENGTH`] for a segment whose length is unknown (and everything after it placed from there).
#[must_use]
pub fn layout(described: &Value) -> Vec<PlacedSegment> {
    let mut out = Vec::new();
    for (ti, track) in described["tracks"].as_array().into_iter().flatten().enumerate() {
        let mut at = 0.0_f32;
        for (si, seg) in track["segments"].as_array().into_iter().flatten().enumerate() {
            let known = f(&seg["length"]);
            let length = known.unwrap_or(UNKNOWN_LENGTH).max(0.0);
            out.push(PlacedSegment {
                track: ti,
                segment: si,
                clip: seg["clip"].as_str().unwrap_or_default().to_owned(),
                at,
                length,
                known: known.is_some(),
                start: f(&seg["start"]).unwrap_or(0.0),
                end: f(&seg["end"]),
                rate: f(&seg["rate"]).unwrap_or(1.0),
                loops: seg["loops"]
                    .as_u64()
                    .map_or(1, |l| l.min(u64::from(u32::MAX)) as u32),
            });
            at += length;
        }
    }
    out
}

/// The timeline's length: the describe length, else the end of the last placed segment.
#[must_use]
pub fn timeline_length(described: &Value) -> f32 {
    f(&described["length"]).unwrap_or_else(|| {
        layout(described)
            .iter()
            .map(|s| s.at + s.length)
            .fold(0.0, f32::max)
    })
}

fn sections(described: &Value) -> Vec<(String, f32, Value)> {
    let mut out: Vec<(String, f32, Value)> = described["sections"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            (
                s["name"].as_str().unwrap_or_default().to_owned(),
                f(&s["start"]).unwrap_or(0.0),
                s["next"].clone(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.1.total_cmp(&b.1));
    out
}

/// The sections the preview plays, in order: from `from` (else the first) following each section's link
/// (`continue` to the next on the timeline, `stop`, `jump`), stopping at the first repeat (a loop) or after
/// [`MAX_CHAIN`] steps; with `whole`, every section in timeline order. A montage without sections plays one
/// implicit `default` section over the whole timeline. Returns the steps and whether the chain loops.
///
/// # Errors
/// `montage_panel_section_not_found` when `from` names no section.
pub fn section_chain(
    described: &Value,
    from: Option<&str>,
    whole: bool,
) -> Result<(Vec<ChainStep>, bool), PanelError> {
    let length = timeline_length(described);
    let list = sections(described);
    if list.is_empty() {
        return Ok((
            vec![ChainStep {
                section: "default".to_owned(),
                start: 0.0,
                end: length,
            }],
            false,
        ));
    }
    let end_of = |i: usize| list.get(i + 1).map_or(length, |s| s.1).max(list[i].1);
    let step = |i: usize| ChainStep {
        section: list[i].0.clone(),
        start: list[i].1,
        end: end_of(i),
    };
    if whole {
        return Ok(((0..list.len()).map(step).collect(), false));
    }
    let mut at = match from {
        None => 0,
        Some(name) => list
            .iter()
            .position(|s| s.0 == name)
            .ok_or_else(|| err("section_not_found", format!("no section `{name}`")))?,
    };
    let mut seen = vec![false; list.len()];
    let mut out = Vec::new();
    loop {
        if seen[at] || out.len() >= MAX_CHAIN {
            return Ok((out, true));
        }
        seen[at] = true;
        out.push(step(at));
        let next = &list[at].2;
        at = match next {
            Value::String(s) if s == "stop" => return Ok((out, false)),
            Value::Object(map) => match map.get("jump").and_then(Value::as_str) {
                Some(target) => match list.iter().position(|s| s.0 == target) {
                    Some(i) => i,
                    None => return Ok((out, false)),
                },
                None => return Ok((out, false)),
            },
            // `continue`: the next section on the timeline, or the end.
            _ => {
                if at + 1 >= list.len() {
                    return Ok((out, false));
                }
                at + 1
            }
        };
    }
}

/// What each track plays at montage time `t`: `[{ track, slot, segment, clip, clip_time }]` (a track past
/// its last segment is left out). Clip time follows the segment's rate and wraps its loops.
#[must_use]
pub fn active_at(described: &Value, t: f32) -> Vec<Value> {
    let placed = layout(described);
    let mut out = Vec::new();
    for (ti, track) in described["tracks"].as_array().into_iter().flatten().enumerate() {
        let Some(seg) = placed
            .iter()
            .find(|s| s.track == ti && t >= s.at && t < s.at + s.length.max(f32::EPSILON))
        else {
            continue;
        };
        let local = (t - seg.at) * seg.rate;
        let span = seg
            .end
            .map_or(seg.length * seg.rate / seg.loops.max(1) as f32, |e| e - seg.start);
        let clip_time = if span > 0.0 {
            seg.start + local.rem_euclid(span)
        } else {
            seg.start
        };
        out.push(json!({
            "track": ti,
            "slot": track["slot"],
            "segment": seg.segment,
            "clip": seg.clip,
            "clip_time": clip_time,
        }));
    }
    out
}

impl MontagePanel {
    /// Opens `asset` (project-relative) with `anim.montage_describe`.
    ///
    /// # Errors
    /// The describe error (`bad_path`, `montage_unknown_asset`, `montage_invalid`, ...).
    pub fn open(&mut self, world: &mut World, asset: &str) -> Result<(), PanelError> {
        let described = regia_agent::montage_describe(world, &json!({ "asset": asset }))
            .map_err(|e| from_rpc(&e, "montage_panel_open_failed"))?;
        if self.asset.as_deref() != Some(asset) {
            // The agent's undo stack is per asset: another montage's steps are not this one's.
            self.journal.retain(|step| matches!(step, UndoStep::Slots(_)));
        }
        self.asset = Some(asset.to_owned());
        self.described = Some(described);
        self.reads += 1;
        self.picked = None;
        self.chain_from = None;
        self.playhead = 0.0;
        self.error = None;
        self.refresh_pose(world, true);
        Ok(())
    }

    /// Reads the open asset again (after an edit or an outside change).
    ///
    /// # Errors
    /// `montage_panel_no_asset`, or the describe error.
    pub fn refresh(&mut self, world: &mut World) -> Result<(), PanelError> {
        let asset = self
            .asset
            .clone()
            .ok_or_else(|| err("no_asset", "open a montage first"))?;
        let described = regia_agent::montage_describe(world, &json!({ "asset": asset }))
            .map_err(|e| from_rpc(&e, "montage_panel_open_failed"))?;
        self.described = Some(described);
        self.reads += 1;
        Ok(())
    }

    /// Selects `picked` on the timeline. A new selection loads the follow form from the selected section's
    /// or notify's own link (the default form when it follows nothing), so changing one field of the form
    /// and pressing Set follow keeps the others (rev-claude-64 (4) finding 1). Selecting the item already
    /// selected keeps the form as the user left it.
    pub fn select(&mut self, picked: Option<Picked>) {
        if self.picked != picked {
            self.picked = picked;
            self.seed_follow();
        }
    }

    /// Loads the follow form from the selected section's or notify's link in the last read.
    fn seed_follow(&mut self) {
        let (list, name) = match &self.picked {
            Some(Picked::Section(name)) => ("sections", name),
            Some(Picked::Notify(name)) => ("notifies", name),
            _ => return,
        };
        let link = self
            .described
            .as_ref()
            .and_then(|d| d[list].as_array())
            .into_iter()
            .flatten()
            .find(|item| item["name"] == name.as_str())
            .and_then(|item| serde_json::from_value::<FollowEdit>(item["follow"].clone()).ok());
        self.follow = link.map_or_else(FollowForm::default, |f| FollowForm {
            track: f.track,
            segment: f.segment,
            mode: f.mode,
        });
    }

    /// Applies one `anim.montage_edit` edit (`{ op, ... }`, or `{ op: "undo" }`) to the open asset and reads
    /// it again. Returns the edit's report.
    ///
    /// # Errors
    /// `montage_panel_no_asset`, or the edit's error (`montage_bad_edit`, `montage_invalid`, ...); a refused
    /// edit writes nothing.
    pub fn edit(&mut self, world: &mut World, edit: &Value, dry_run: bool) -> Result<Value, PanelError> {
        let _span = tracing::debug_span!(target: "regia_editor", "montage_panel_edit").entered();
        let undo = edit.get("op").and_then(Value::as_str) == Some("undo");
        if undo && !dry_run && matches!(self.journal.last(), Some(UndoStep::Slots(_))) {
            return self.undo_slots(world);
        }
        let asset = self
            .asset
            .clone()
            .ok_or_else(|| err("no_asset", "open a montage first"))?;
        let report = regia_agent::montage_edit(
            world,
            &json!({ "asset": asset, "edit": edit, "dry_run": dry_run }),
        )
        .map_err(|e| from_rpc(&e, "montage_bad_edit"))?;
        if !dry_run {
            if undo {
                if matches!(self.journal.last(), Some(UndoStep::Montage(_))) {
                    self.journal.pop();
                }
            } else {
                self.push_step(UndoStep::Montage(asset.clone()));
            }
            self.refresh(world)?;
            // A removed item leaves the selection.
            if let Some(Picked::Segment { track, segment }) = &self.picked
                && !layout(self.described.as_ref().unwrap_or(&Value::Null))
                    .iter()
                    .any(|s| s.track == *track && s.segment == *segment)
            {
                self.picked = None;
            }
            // The selected item's link may have changed (Set follow, Undo): show it.
            self.seed_follow();
        }
        self.last_edit = Some(report.clone());
        self.refresh_pose(world, true);
        Ok(report)
    }

    /// Sets the preview: `playhead` (clamped to the timeline), `section` (the chain start, `null` = the first)
    /// and `whole`.
    ///
    /// # Errors
    /// `montage_panel_no_asset`, `montage_panel_section_not_found`; nothing changes.
    pub fn set_preview(&mut self, params: &Value) -> Result<(), PanelError> {
        let described = self
            .described
            .as_ref()
            .ok_or_else(|| err("no_asset", "open a montage first"))?;
        let playhead = opt_f32(params, "playhead").map_err(|e| err("bad_value", e.message))?;
        let whole = opt_bool(params, "whole").map_err(|e| err("bad_value", e.message))?;
        let section = match params.get("section") {
            None => self.chain_from.clone(),
            Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err(err("bad_value", "`section` must be a section name or null")),
        };
        section_chain(described, section.as_deref(), false)?;
        let length = timeline_length(described);
        if let Some(t) = playhead {
            self.playhead = t.clamp(0.0, length);
        }
        if let Some(whole) = whole {
            self.whole = whole;
        }
        self.chain_from = section;
        Ok(())
    }

    /// Picks the rig of the pose preview (`entity`, or `null` to clear) and samples it.
    ///
    /// # Errors
    /// `montage_panel_bad_value`, `montage_panel_no_rig` (no `SkinnedMesh`); nothing changes.
    pub fn set_rig(&mut self, world: &World, value: &Value) -> Result<(), PanelError> {
        self.rig = match value {
            Value::Null => None,
            v => {
                let e = v
                    .as_u64()
                    .and_then(Entity::try_from_bits)
                    .ok_or_else(|| err("bad_value", "`rig` must be an entity id or null"))?;
                if world.get::<regia_animation::SkinnedMesh>(e).is_none() {
                    return Err(err(
                        "no_rig",
                        format!("entity {} has no SkinnedMesh", e.to_bits()),
                    ));
                }
                Some(e)
            }
        };
        self.refresh_pose(world, true);
        Ok(())
    }

    /// Reads the slot table at `path` (project-relative `.slots.ron`) with `anim.montage_slots` into the
    /// slot-table editor.
    ///
    /// # Errors
    /// The read's error (`bad_path`, `montage_unknown_asset`, `montage_invalid`); the editor keeps its table.
    pub fn load_slots(&mut self, world: &mut World, path: &str) -> Result<(), PanelError> {
        let table = regia_agent::montage_slots(world, &json!({ "path": path }))
            .map_err(|e| from_rpc(&e, "montage_panel_open_failed"))?;
        self.slots.path = table["path"].as_str().map(str::to_owned);
        self.slots.table = Some(table);
        self.slots.rename.clear();
        self.slots.set_forms.clear();
        Ok(())
    }

    /// Applies one `anim.montage_slot_edit` op (`slot_add_op`, `slot_rename_op`, `slot_delete_op`, or a
    /// `set` op) to the slot-table editor's table, reads the table again, and reads the open montage again
    /// (a rename rewrites the montages that name the slot). Returns the edit's answer.
    ///
    /// # Errors
    /// `montage_panel_no_slots` (no table loaded), or the edit's error (`montage_bad_edit`, `slot_in_use`,
    /// `slot_unknown`, `montage_read_only`, ...); a refused edit writes nothing.
    pub fn slot_edit(&mut self, world: &mut World, op: &Value, force: bool) -> Result<Value, PanelError> {
        let _span = tracing::debug_span!(target: "regia_editor", "montage_panel_slot_edit").entered();
        let path = self
            .slots
            .path
            .clone()
            .ok_or_else(|| err("no_slots", "load a .slots.ron first"))?;
        // The texts the edit may replace, for its undo step: the table, and for a rename every montage
        // and graph that names the slot (the files the edit rewrites, read the way it finds them).
        let root = project_root(world);
        let mut before: Vec<(String, Option<String>)> = vec![(path.clone(), read_rel(&root, &path))];
        if op.get("op").and_then(Value::as_str) == Some("rename")
            && let Some(from) = op.get("from").and_then(Value::as_str)
        {
            let table = regia_agent::montage_slots(world, &json!({ "path": path }))
                .map_err(|e| from_rpc(&e, "montage_panel_open_failed"))?;
            for list in ["used_by", "used_by_graphs"] {
                for file in table[list][from].as_array().into_iter().flatten() {
                    if let Some(file) = file.as_str()
                        && before.iter().all(|(p, _)| p != file)
                    {
                        before.push((file.to_owned(), read_rel(&root, file)));
                    }
                }
            }
        }
        let report =
            regia_agent::montage_slot_edit(world, &json!({ "path": path, "op": op, "force": force }))
                .map_err(|e| from_rpc(&e, "montage_bad_edit"))?;
        let rewritten: Vec<&str> = report["rewritten"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        before.retain(|(p, _)| *p == path || rewritten.contains(&p.as_str()));
        if let Some(missing) = rewritten.iter().find(|r| before.iter().all(|(p, _)| p != *r)) {
            tracing::warn!(target: "regia_editor", "montage slot edit rewrote {missing}, which its undo step does not cover");
        }
        // A rewritten open montage lost its agent undo steps (`anim.montage_slot_edit` drops them):
        // its journal steps go too.
        if let Some(asset) = &self.asset
            && rewritten.contains(&asset.as_str())
        {
            self.journal
                .retain(|step| !matches!(step, UndoStep::Montage(a) if a == asset));
        }
        self.push_step(UndoStep::Slots(before));
        self.load_slots(world, &path)?;
        if self.asset.is_some() {
            self.refresh(world)?;
            self.refresh_pose(world, true);
        }
        Ok(report)
    }

    /// How many steps the panel's undo journal holds.
    #[must_use]
    pub fn journal_depth(&self) -> usize {
        self.journal.len()
    }

    fn push_step(&mut self, step: UndoStep) {
        if self.journal.len() == JOURNAL_DEPTH {
            self.journal.remove(0);
        }
        self.journal.push(step);
    }

    /// Undoes the newest journal step, a slot-table edit: puts back every file it wrote (removes one
    /// it created), then reads the table and the open montage again.
    fn undo_slots(&mut self, world: &mut World) -> Result<Value, PanelError> {
        let Some(UndoStep::Slots(files)) = self.journal.pop() else {
            return Err(err("nothing_to_undo", "the newest step is not a slot-table edit"));
        };
        let root = project_root(world);
        let mut restored = Vec::new();
        for (rel, text) in &files {
            let full = root.join(rel);
            let result = match text {
                Some(text) => crate::data_panels::write_atomic(&full, text, "montage_panel"),
                None => match std::fs::remove_file(&full) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(err("io_failed", format!("{rel}: {e}"))),
                },
            };
            if let Err(e) = result {
                // Keep the step so a second Undo can try again.
                self.journal.push(UndoStep::Slots(files.clone()));
                return Err(e);
            }
            restored.push(rel.clone());
        }
        if let Some(path) = self.slots.path.clone() {
            self.load_slots(world, &path)?;
        }
        if self.asset.is_some() {
            self.refresh(world)?;
            self.seed_follow();
            self.refresh_pose(world, true);
        }
        let report = json!({
            "op": "undo",
            "kind": "slots",
            "restored": restored,
            "saved": true,
            "journal_depth": self.journal.len(),
        });
        self.last_edit = Some(report.clone());
        Ok(report)
    }

    /// The panel as the agent reads it.
    #[must_use]
    pub fn json(&self) -> Value {
        let preview = self.described.as_ref().map(|d| {
            let (chain, loops) = section_chain(d, self.chain_from.as_deref(), self.whole).unwrap_or_default();
            json!({
                "playhead": self.playhead,
                "length": timeline_length(d),
                "section": self.chain_from,
                "whole": self.whole,
                "chain": chain.iter().map(|s| json!({ "section": s.section, "start": s.start, "end": s.end })).collect::<Vec<_>>(),
                "loops": loops,
                "active": active_at(d, self.playhead),
                "rig": self.rig.map(Entity::to_bits),
                "pose": self.pose.as_ref().map(|p| match p {
                    Ok(p) => json!({
                        "sampled": p.sampled,
                        "joints": p.joints.len(),
                        "positions": p.joints.iter().map(|v| [v.x, v.y, v.z]).collect::<Vec<_>>(),
                    }),
                    Err(e) => json!({ "error": e.json() }),
                }),
            })
        });
        json!({
            "asset": self.asset,
            "montage": self.described,
            "layout": self.described.as_ref().map(|d| layout(d).iter().map(|s| json!({
                "track": s.track, "segment": s.segment, "clip": s.clip, "at": s.at, "length": s.length,
                "known": s.known,
            })).collect::<Vec<_>>()),
            "picked": self.picked.as_ref().map(Picked::json),
            "preview": preview,
            "last_edit": self.last_edit,
            "journal_depth": self.journal.len(),
            "slots": self.slots.table,
            "error": self.error.as_ref().map(PanelError::json),
        })
    }
}

/// The rig's pose at the playhead: the main track (track 0) sampled over the bind pose with the clips of the
/// rig's montage player and animator, as joint positions in world space. Slot masks of other tracks are not
/// applied.
#[derive(Clone, Debug, PartialEq)]
pub struct PosePreview {
    /// The rig.
    pub rig: Entity,
    /// World position of each joint.
    pub joints: Vec<regia_core::math::Vec3>,
    /// Parent of each joint.
    pub parents: Vec<Option<u32>>,
    /// Whether the main track had a clip at the playhead (else the bind pose).
    pub sampled: bool,
}

fn project_root(world: &World) -> std::path::PathBuf {
    world
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf())
        .or_else(|| crate::mesh_source::project_root(world))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Samples `asset` at `playhead` on `rig` (see [`PosePreview`]).
///
/// # Errors
/// `montage_panel_no_rig`, `montage_panel_preview_variant` (a child montage), `montage_panel_bad_path`,
/// `montage_panel_file_not_found`, `montage_invalid`, `montage_panel_missing_clip`.
pub fn pose_preview(
    world: &World,
    asset: &str,
    rig: Entity,
    playhead: f32,
) -> Result<PosePreview, PanelError> {
    let _span = tracing::debug_span!(target: "regia_editor", "montage_panel_pose").entered();
    let mesh = world
        .get::<regia_animation::SkinnedMesh>(rig)
        .ok_or_else(|| err("no_rig", format!("entity {} has no SkinnedMesh", rig.to_bits())))?;
    if !asset.to_ascii_lowercase().ends_with(".montage.ron") {
        return Err(err(
            "preview_variant",
            "the pose preview samples a .montage.ron (open the parent of a child montage)",
        ));
    }
    let file = crate::data_panels::confined(&project_root(world), asset, &[".montage.ron"], "montage_panel")?;
    let text = std::fs::read_to_string(&file).map_err(|e| err("file_not_found", format!("{asset}: {e}")))?;
    let montage = regia_animation::Montage::from_ron_str(&text)
        .map_err(|e| PanelError::new("montage_invalid", format!("{asset}: {e}")))?;
    let mut libraries = Vec::new();
    if let Some(player) = world.get::<regia_animation::MontagePlayer>(rig) {
        libraries.push(player.clips.clone());
    }
    if let Some(animator) = world.get::<regia_animation::Animator>(rig)
        && let regia_animation::AnimatorSource::Graph { clips, .. } = &animator.source
    {
        libraries.push(clips.clone());
    }
    let layout = montage
        .layout(|name| libraries.iter().find_map(|l| l.get(name).cloned()))
        .map_err(|e| err("missing_clip", e.to_string()))?;
    let skeleton = &mesh.skeleton;
    let bind = skeleton.bind_pose_pose();
    let mut pose = bind.clone();
    let sampled = layout.sample(0, playhead, &bind, true, &mut pose);
    let base = crate::ragdoll_panel::rig_matrix(world, rig).unwrap_or(regia_core::math::Mat4::IDENTITY);
    let joints = skeleton
        .globals(&pose, Some(base))
        .iter()
        .map(|g| g.w_axis.truncate())
        .collect();
    Ok(PosePreview {
        rig,
        joints,
        parents: skeleton.parents.clone(),
        sampled,
    })
}

impl MontagePanel {
    /// Samples the pose preview again when the rig, the playhead or the montage changed (or `force`).
    pub fn refresh_pose(&mut self, world: &World, force: bool) {
        let key = (self.rig.map(Entity::to_bits), self.playhead.to_bits(), self.reads);
        if !force && self.pose_key == Some(key) {
            return;
        }
        self.pose_key = Some(key);
        self.pose = match (self.rig, &self.asset) {
            (Some(rig), Some(asset)) => Some(pose_preview(world, asset, rig, self.playhead)),
            _ => None,
        };
    }
}

/// The pose preview as viewport lines, joint to parent (empty without a preview).
#[must_use]
pub fn montage_pose_segments(world: &World) -> Vec<crate::audio_gizmo::Segment> {
    let Some(Some(Ok(pose))) = world.get_resource::<MontagePanel>().map(|p| p.pose.as_ref()) else {
        return Vec::new();
    };
    let color = egui::Color32::from_rgb(120, 240, 200);
    pose.parents
        .iter()
        .enumerate()
        .filter_map(|(j, p)| {
            let p = (*p)? as usize;
            Some((*pose.joints.get(p)?, *pose.joints.get(j)?, color))
        })
        .collect()
}

/// The clip name a dropped asset path gives: its file name up to the first dot.
#[must_use]
pub fn clip_name_of(path: &str) -> String {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.split('.').next().unwrap_or(file).to_owned()
}

// ------------------------------------------------------------------------------------------- drawing

const LANE: f32 = 22.0;
const LABEL: f32 = 90.0;

fn segment_color(track: usize) -> egui::Color32 {
    [
        egui::Color32::from_rgb(70, 120, 190),
        egui::Color32::from_rgb(90, 160, 110),
        egui::Color32::from_rgb(170, 120, 70),
        egui::Color32::from_rgb(140, 90, 170),
    ][track % 4]
}

/// Draws the panel.
pub fn montage_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<MontagePanel>().unwrap_or_default();
    let mut edits: Vec<Value> = Vec::new();
    draw(ui, &mut panel, &mut edits);
    if std::mem::take(&mut panel.open_requested) {
        let asset = panel.path_input.trim().to_owned();
        if let Err(e) = panel.open(world, &asset) {
            panel.error = Some(e);
        }
    }
    for edit in edits {
        match panel.edit(world, &edit, false) {
            Ok(_) => panel.error = None,
            Err(e) => {
                panel.error = Some(e);
                break;
            }
        }
    }
    for request in std::mem::take(&mut panel.slot_requests) {
        let result = match request {
            SlotRequest::Load(path) => panel.load_slots(world, &path),
            SlotRequest::Edit { op, force } => panel.slot_edit(world, &op, force).map(|_| ()),
        };
        match result {
            Ok(()) => panel.error = None,
            Err(e) => {
                panel.error = Some(e);
                break;
            }
        }
    }
    if std::mem::take(&mut panel.use_selected_rig) {
        let selected = world
            .get_resource::<crate::selection::Selection>()
            .and_then(crate::selection::Selection::primary)
            .map_or(Value::Null, |e| json!(e.to_bits()));
        if let Err(e) = panel.set_rig(world, &selected) {
            panel.error = Some(e);
        }
    }
    panel.refresh_pose(world, false);
    world.insert_resource(panel);
}

fn draw(ui: &mut egui::Ui, panel: &mut MontagePanel, edits: &mut Vec<Value>) {
    crate::data_panels::header(ui, "Montage", "slot tracks · sections · notifies · warps");
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut panel.path_input).hint_text("assets/anim/combo.montage.ron"));
        if ui.button("Open").clicked() {
            panel.open_requested = true;
        }
        if (panel.asset.is_some() || !panel.journal.is_empty()) && ui.button("Undo").clicked() {
            edits.push(json!({ "op": "undo" }));
        }
    });
    if let Some(e) = &panel.error {
        e.draw(ui);
    }
    let Some(described) = panel.described.clone() else {
        ui.weak("Open a .montage.ron or .montagevar.ron.");
        return;
    };
    let placed = layout(&described);
    let length = timeline_length(&described).max(0.5);
    let (chain, loops) =
        section_chain(&described, panel.chain_from.as_deref(), panel.whole).unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label("Playhead");
        ui.add(egui::Slider::new(&mut panel.playhead, 0.0..=length).suffix(" s"));
        ui.checkbox(&mut panel.whole, "Whole timeline");
        if ui.button("Preview on selected rig").clicked() {
            panel.use_selected_rig = true;
        }
        match &panel.pose {
            Some(Ok(p)) => ui.weak(if p.sampled { "posed" } else { "bind pose" }),
            Some(Err(e)) => ui
                .colored_label(
                    egui::Color32::from_rgb(230, 170, 80),
                    crate::data_panels::sentence(&e.message),
                )
                .on_hover_text(format!("Agent error code: {}", e.code)),
            None => ui.weak("no rig"),
        };
        ui.label(format!(
            "plays: {}{}",
            chain
                .iter()
                .map(|s| s.section.as_str())
                .collect::<Vec<_>>()
                .join(" → "),
            if loops { " ↺" } else { "" }
        ));
    });
    let tracks = described["tracks"].as_array().cloned().unwrap_or_default();
    let lanes = tracks.len() + 3;
    let width = (LABEL + length * panel.zoom + 20.0).max(ui.available_width());
    egui::ScrollArea::horizontal()
        .id_salt("montage-lanes")
        .show(ui, |ui| {
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(width, LANE * (lanes as f32 + 1.0)),
                egui::Sense::click_and_drag(),
            );
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 0.0, egui::Color32::from_gray(24));
            let zoom = panel.zoom.max(1.0);
            let x_of = |t: f32| rect.left() + LABEL + t * zoom;
            let t_of = |x: f32| ((x - rect.left() - LABEL) / zoom).max(0.0);
            let lane_y = |i: usize| rect.top() + LANE * (i as f32 + 1.0);
            let font = egui::FontId::monospace(10.0);
            let text = egui::Color32::from_gray(200);
            // Ruler.
            let mut t = 0.0;
            while t <= length + 1e-3 {
                painter.line_segment(
                    [
                        egui::pos2(x_of(t), rect.top()),
                        egui::pos2(x_of(t), rect.bottom()),
                    ],
                    egui::Stroke::new(1.0, egui::Color32::from_gray(45)),
                );
                painter.text(
                    egui::pos2(x_of(t) + 2.0, rect.top() + 2.0),
                    egui::Align2::LEFT_TOP,
                    format!("{t:.1}"),
                    font.clone(),
                    text,
                );
                t += 0.5;
            }
            // The previewed chain, under everything.
            for step in &chain {
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x_of(step.start), rect.top() + 14.0),
                        egui::pos2(x_of(step.end), rect.top() + LANE - 2.0),
                    ),
                    0.0,
                    egui::Color32::from_rgb(60, 70, 40),
                );
            }
            // Track lanes.
            for (ti, track) in tracks.iter().enumerate() {
                let y = lane_y(ti);
                painter.text(
                    egui::pos2(rect.left() + 4.0, y + 4.0),
                    egui::Align2::LEFT_TOP,
                    track["slot"].as_str().unwrap_or("?"),
                    font.clone(),
                    text,
                );
                for s in placed.iter().filter(|s| s.track == ti) {
                    let r = egui::Rect::from_min_max(
                        egui::pos2(x_of(s.at), y + 2.0),
                        egui::pos2(x_of(s.at + s.length), y + LANE - 2.0),
                    );
                    let selected = panel.picked
                        == Some(Picked::Segment {
                            track: s.track,
                            segment: s.segment,
                        });
                    painter.rect_filled(r, 2.0, segment_color(ti));
                    if !s.known {
                        painter.rect_stroke(
                            r,
                            2.0,
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(230, 170, 60)),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if selected {
                        painter.rect_stroke(
                            r,
                            2.0,
                            egui::Stroke::new(2.0, egui::Color32::WHITE),
                            egui::StrokeKind::Inside,
                        );
                    }
                    painter.text(
                        r.left_center() + egui::vec2(3.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        &s.clip,
                        font.clone(),
                        egui::Color32::WHITE,
                    );
                }
            }
            // Sections, notifies, warps.
            let sy = lane_y(tracks.len());
            let ny = lane_y(tracks.len() + 1);
            let wy = lane_y(tracks.len() + 2);
            for (label, y) in [("sections", sy), ("notifies", ny), ("warps", wy)] {
                painter.text(
                    egui::pos2(rect.left() + 4.0, y + 4.0),
                    egui::Align2::LEFT_TOP,
                    label,
                    font.clone(),
                    text,
                );
            }
            for (name, start, _) in sections(&described) {
                let x = x_of(start);
                let color = if panel.picked == Some(Picked::Section(name.clone())) {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_rgb(230, 200, 90)
                };
                painter.line_segment(
                    [egui::pos2(x, sy), egui::pos2(x, sy + LANE)],
                    egui::Stroke::new(2.0, color),
                );
                painter.text(
                    egui::pos2(x + 3.0, sy + 4.0),
                    egui::Align2::LEFT_TOP,
                    &name,
                    font.clone(),
                    color,
                );
            }
            for n in described["notifies"].as_array().into_iter().flatten() {
                let (Some(name), Some(time)) = (n["name"].as_str(), f(&n["time"])) else {
                    continue;
                };
                let duration = f(&n["duration"]).unwrap_or(0.0);
                let color = if panel.picked == Some(Picked::Notify(name.to_owned())) {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_rgb(120, 200, 230)
                };
                if duration > 0.0 {
                    painter.rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(x_of(time), ny + 6.0),
                            egui::pos2(x_of(time + duration), ny + LANE - 6.0),
                        ),
                        1.0,
                        color,
                    );
                }
                painter.circle_filled(egui::pos2(x_of(time), ny + LANE * 0.5), 4.0, color);
                painter.text(
                    egui::pos2(x_of(time) + 6.0, ny + 2.0),
                    egui::Align2::LEFT_TOP,
                    name,
                    font.clone(),
                    color,
                );
            }
            for (i, w) in described["warps"].as_array().into_iter().flatten().enumerate() {
                let (Some(a), Some(b)) = (f(&w["start"]), f(&w["end"])) else {
                    continue;
                };
                let color = if panel.picked == Some(Picked::Warp(i)) {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_rgb(200, 110, 110)
                };
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x_of(a), wy + 4.0),
                        egui::pos2(x_of(b), wy + LANE - 4.0),
                    ),
                    2.0,
                    color,
                );
                painter.text(
                    egui::pos2(x_of(a) + 3.0, wy + 4.0),
                    egui::Align2::LEFT_TOP,
                    w["target"].as_str().unwrap_or(""),
                    font.clone(),
                    egui::Color32::BLACK,
                );
            }
            // Playhead.
            painter.line_segment(
                [
                    egui::pos2(x_of(panel.playhead), rect.top()),
                    egui::pos2(x_of(panel.playhead), rect.bottom()),
                ],
                egui::Stroke::new(1.5, egui::Color32::from_rgb(240, 80, 80)),
            );
            // Gestures: click picks; dragging a section marker moves it; dragging the ruler scrubs.
            let lane_at = |y: f32| ((y - rect.top()) / LANE).floor() as isize - 1;
            let pick = |p: egui::Pos2| -> Option<Picked> {
                let lane = lane_at(p.y);
                let t = t_of(p.x);
                if lane < 0 {
                    return None;
                }
                let lane = lane as usize;
                if lane < tracks.len() {
                    return placed
                        .iter()
                        .find(|s| s.track == lane && t >= s.at && t <= s.at + s.length)
                        .map(|s| Picked::Segment {
                            track: s.track,
                            segment: s.segment,
                        });
                }
                let near = |x: f32| (x_of(x) - p.x).abs() < 6.0;
                match lane - tracks.len() {
                    0 => sections(&described)
                        .into_iter()
                        .find(|s| near(s.1))
                        .map(|s| Picked::Section(s.0)),
                    1 => described["notifies"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find_map(|n| {
                            let time = f(&n["time"])?;
                            let end = time + f(&n["duration"]).unwrap_or(0.0);
                            (near(time) || (t >= time && t <= end))
                                .then(|| Picked::Notify(n["name"].as_str().unwrap_or_default().to_owned()))
                        }),
                    2 => described["warps"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .enumerate()
                        .find_map(|(i, w)| {
                            (t >= f(&w["start"])? && t <= f(&w["end"])?).then_some(Picked::Warp(i))
                        }),
                    _ => None,
                }
            };
            if response.clicked()
                && let Some(p) = response.interact_pointer_pos()
            {
                if lane_at(p.y) < 0 {
                    panel.playhead = t_of(p.x).min(length);
                } else {
                    panel.select(pick(p));
                }
            }
            // A drag on the ruler scrubs; a drag that starts on a section marker moves that section.
            if response.drag_started()
                && let Some(p) = ui.input(|i| i.pointer.press_origin())
            {
                panel.drag = if lane_at(p.y) < 0 {
                    Some(TimelineDrag::Scrub)
                } else {
                    match pick(p) {
                        Some(Picked::Section(name)) => {
                            panel.select(Some(Picked::Section(name.clone())));
                            Some(TimelineDrag::Section(name))
                        }
                        other => {
                            panel.select(other);
                            None
                        }
                    }
                };
            }
            if response.dragged()
                && let (Some(TimelineDrag::Scrub), Some(p)) = (&panel.drag, response.interact_pointer_pos())
            {
                panel.playhead = t_of(p.x).min(length);
            }
            if response.drag_stopped() {
                if let (Some(TimelineDrag::Section(name)), Some(p)) =
                    (&panel.drag, response.interact_pointer_pos())
                {
                    edits.push(json!({ "op": "set_section", "name": name, "start": t_of(p.x) }));
                }
                panel.drag = None;
            }
            // A clip dropped on a track lane joins that track at its end.
            if let Some(drop) = response.dnd_release_payload::<crate::dnd::ContentDrag>()
                && let Some(p) = response.interact_pointer_pos().or(response.hover_pos())
                && let Ok(lane) = usize::try_from(lane_at(p.y))
                && let Some(track) = tracks.get(lane)
            {
                edits.push(
                    json!({ "op": "add_segment", "slot": track["slot"], "clip": clip_name_of(&drop.path) }),
                );
            }
        });
    // The selected item.
    match panel.picked.clone() {
        Some(Picked::Segment { track, segment }) => {
            if let Some(s) = placed.iter().find(|s| s.track == track && s.segment == segment) {
                let mut s = s.clone();
                ui.horizontal(|ui| {
                    ui.strong(format!("{} (track {track}, segment {segment})", s.clip));
                    let mut changed = false;
                    ui.label("start");
                    changed |= ui.add(egui::DragValue::new(&mut s.start).speed(0.01).range(0.0..=1e6)).changed();
                    ui.label("rate");
                    changed |= ui.add(egui::DragValue::new(&mut s.rate).speed(0.01).range(0.01..=20.0)).changed();
                    ui.label("loops");
                    changed |= ui.add(egui::DragValue::new(&mut s.loops).range(1..=1000)).changed();
                    if changed {
                        edits.push(json!({ "op": "move_segment", "track": track, "segment": segment, "start": s.start, "rate": s.rate, "loops": s.loops }));
                    }
                    if ui.small_button("◀").clicked() && segment > 0 {
                        edits.push(json!({ "op": "move_segment", "track": track, "segment": segment, "to": segment - 1 }));
                    }
                    if ui.small_button("▶").clicked() {
                        edits.push(json!({ "op": "move_segment", "track": track, "segment": segment, "to": segment + 1 }));
                    }
                    if ui.small_button("Remove").clicked() {
                        edits.push(json!({ "op": "remove_segment", "track": track, "segment": segment }));
                    }
                });
            }
        }
        Some(Picked::Section(name)) => {
            ui.horizontal(|ui| {
                ui.strong(format!("section {name}"));
                if ui.button("Preview from here").clicked() {
                    panel.chain_from = Some(name.clone());
                    panel.whole = false;
                }
                if ui.button("Stop at end").clicked() {
                    edits.push(json!({ "op": "link_sections", "from": name, "stop": true }));
                }
                if ui.button("Remove").clicked() {
                    edits.push(json!({ "op": "remove_section", "name": name }));
                }
            });
            link_row(ui, panel, &described, &name, edits);
            follow_row(ui, panel, &described, "section", &name, edits);
        }
        Some(Picked::Notify(name)) => {
            ui.horizontal(|ui| {
                ui.strong(format!("notify {name}"));
                if ui.button("Remove").clicked() {
                    edits.push(json!({ "op": "remove_notify", "name": name }));
                }
            });
            follow_row(ui, panel, &described, "notify", &name, edits);
        }
        Some(Picked::Warp(index)) => {
            ui.horizontal(|ui| {
                ui.strong(format!("warp {index}"));
                if ui.button("Remove").clicked() {
                    edits.push(json!({ "op": "remove_warp", "index": index }));
                }
            });
        }
        None => {}
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut panel.add_clip)
                .hint_text("clip")
                .desired_width(90.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut panel.add_slot)
                .hint_text("slot (a new one adds a track)")
                .desired_width(90.0),
        );
        let typed = panel.add_slot.trim();
        let slot = if typed.is_empty() {
            tracks.first().and_then(|t| t["slot"].as_str()).map(str::to_owned)
        } else {
            Some(typed.to_owned())
        };
        if let Some(slot) = slot
            && ui.button(format!("Add to {slot}")).clicked()
            && !panel.add_clip.trim().is_empty()
        {
            edits.push(add_segment_edit(&slot, panel.add_clip.trim()));
        }
        ui.add(
            egui::TextEdit::singleline(&mut panel.add_notify)
                .hint_text("notify")
                .desired_width(90.0),
        );
        if ui.button("Add notify at playhead").clicked() && !panel.add_notify.trim().is_empty() {
            edits
                .push(json!({ "op": "add_notify", "name": panel.add_notify.trim(), "time": panel.playhead }));
        }
        if ui.button("Section at playhead").clicked() {
            let name = format!("section_{}", sections(&described).len());
            edits.push(json!({ "op": "set_section", "name": name, "start": panel.playhead }));
        }
    });
    warp_row(ui, panel, edits);
    blend_row(ui, panel, &described, edits);
    slot_table(ui, panel);
    let active = active_at(&described, panel.playhead);
    for a in &active {
        ui.weak(format!(
            "{}: {} at {:.3} s",
            a["slot"].as_str().unwrap_or("?"),
            a["clip"].as_str().unwrap_or("?"),
            a["clip_time"].as_f64().unwrap_or(0.0)
        ));
    }
    for issue in described["issues"].as_array().into_iter().flatten() {
        ui.colored_label(
            egui::Color32::from_rgb(230, 170, 80),
            format!(
                "{}: {}",
                issue["code"].as_str().unwrap_or("?"),
                issue["detail"].as_str().unwrap_or("")
            ),
        );
    }
}

/// Where the selected section goes at its end: a jump to another section, or on into the next one.
fn link_row(
    ui: &mut egui::Ui,
    panel: &mut MontagePanel,
    described: &Value,
    name: &str,
    edits: &mut Vec<Value>,
) {
    let others: Vec<String> = sections(described)
        .into_iter()
        .map(|s| s.0)
        .filter(|s| s != name)
        .collect();
    if !others.contains(&panel.link_to) {
        panel.link_to = others.first().cloned().unwrap_or_default();
    }
    let next = described["sections"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["name"] == name)
        .map(|s| match &s["next"] {
            Value::Object(map) => format!("jump {}", map.get("jump").and_then(Value::as_str).unwrap_or("?")),
            other => other.as_str().unwrap_or("continue").to_owned(),
        })
        .unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label(format!("then: {next}"));
        egui::ComboBox::from_id_salt("montage-link-to")
            .selected_text(if panel.link_to.is_empty() {
                "section…"
            } else {
                panel.link_to.as_str()
            })
            .show_ui(ui, |ui| {
                for other in &others {
                    ui.selectable_value(&mut panel.link_to, other.clone(), other.as_str());
                }
            });
        if ui.button("Jump to").clicked() && !panel.link_to.is_empty() {
            edits.push(link_edit(name, Some(&panel.link_to), false));
        }
        if ui.button("Continue").clicked() {
            edits.push(link_edit(name, None, false));
        }
    });
}

/// The follow link of the selected section or notify: the segment it moves with, and how.
fn follow_row(
    ui: &mut egui::Ui,
    panel: &mut MontagePanel,
    described: &Value,
    kind: &str,
    name: &str,
    edits: &mut Vec<Value>,
) {
    let list = if kind == "section" { "sections" } else { "notifies" };
    let current = described[list]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["name"] == name)
        .map(|s| s["follow"].clone())
        .unwrap_or(Value::Null);
    ui.horizontal(|ui| {
        match &current {
            Value::Object(_) => ui.label(format!(
                "follows track {} segment {} ({})",
                current["track"],
                current["segment"],
                current["mode"].as_str().unwrap_or("?")
            )),
            _ => ui.weak("follows nothing"),
        };
        ui.label("track");
        ui.add(egui::DragValue::new(&mut panel.follow.track).range(0..=64));
        ui.label("segment");
        ui.add(egui::DragValue::new(&mut panel.follow.segment).range(0..=4096));
        for (mode, label) in [
            (FollowModeEdit::KeepTime, "keep time"),
            (FollowModeEdit::ShiftWithStart, "shift with start"),
            (FollowModeEdit::ScaleWithLength, "scale with length"),
        ] {
            ui.selectable_value(&mut panel.follow.mode, mode, label);
        }
        if ui.button("Set follow").clicked() {
            edits.push(follow_edit(kind, name, Some(&panel.follow)));
        }
        if current.is_object() && ui.button("Clear follow").clicked() {
            edits.push(follow_edit(kind, name, None));
        }
    });
}

/// The warp-window form: target, window and what it warps.
fn warp_row(ui: &mut egui::Ui, panel: &mut MontagePanel, edits: &mut Vec<Value>) {
    ui.horizontal(|ui| {
        ui.label("Warp");
        ui.add(
            egui::TextEdit::singleline(&mut panel.warp.target)
                .hint_text("target")
                .desired_width(90.0),
        );
        ui.add(
            egui::DragValue::new(&mut panel.warp.start)
                .speed(0.01)
                .range(0.0..=1e6)
                .prefix("from "),
        );
        ui.add(
            egui::DragValue::new(&mut panel.warp.end)
                .speed(0.01)
                .range(0.0..=1e6)
                .prefix("to "),
        );
        ui.checkbox(&mut panel.warp.translation, "translation");
        ui.checkbox(&mut panel.warp.vertical, "vertical");
        ui.checkbox(&mut panel.warp.rotation, "rotation");
        if ui.button("Add warp").clicked() && panel.warp.complete() {
            edits.push(panel.warp.edit());
        }
    });
    warp_m2_row(ui, &mut panel.warp);
}

/// The warp form's spec 34 M2 fields: mode, rotation kind and fraction, and the anchor.
fn warp_m2_row(ui: &mut egui::Ui, warp: &mut WarpForm) {
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        egui::ComboBox::from_id_salt("montage-warp-mode")
            .selected_text(warp_mode_name(warp.mode))
            .show_ui(ui, |ui| {
                for mode in [WarpMode::Similarity, WarpMode::PathScale] {
                    ui.selectable_value(&mut warp.mode, mode, warp_mode_name(mode));
                }
            })
            .response
            .on_hover_text("warp mode");
        egui::ComboBox::from_id_salt("montage-warp-rotation-kind")
            .selected_text(rotation_kind_name(warp.rotation_kind))
            .show_ui(ui, |ui| {
                for kind in [
                    RotationKind::Auto,
                    RotationKind::MatchTarget,
                    RotationKind::FaceTarget,
                ] {
                    ui.selectable_value(&mut warp.rotation_kind, kind, rotation_kind_name(kind));
                }
            })
            .response
            .on_hover_text("rotation kind");
        ui.add(
            egui::DragValue::new(&mut warp.rotation_fraction)
                .speed(0.01)
                .range(0.01..=1.0)
                .prefix("turn over "),
        );
        let anchor_name = match &warp.anchor {
            WarpAnchor::Root => "root",
            WarpAnchor::Bone(_) => "bone",
            WarpAnchor::Offset(_) => "offset",
        };
        egui::ComboBox::from_id_salt("montage-warp-anchor")
            .selected_text(format!("anchor {anchor_name}"))
            .show_ui(ui, |ui| {
                if ui.selectable_label(anchor_name == "root", "root").clicked() {
                    warp.anchor = WarpAnchor::Root;
                }
                if ui.selectable_label(anchor_name == "bone", "bone").clicked() && anchor_name != "bone" {
                    warp.anchor = WarpAnchor::Bone(String::new());
                }
                if ui.selectable_label(anchor_name == "offset", "offset").clicked() && anchor_name != "offset"
                {
                    warp.anchor = WarpAnchor::Offset([0.0; 3]);
                }
            });
        match &mut warp.anchor {
            WarpAnchor::Root => {}
            WarpAnchor::Bone(bone) => {
                ui.add(
                    egui::TextEdit::singleline(bone)
                        .hint_text("anchor bone")
                        .desired_width(90.0),
                );
            }
            WarpAnchor::Offset(v) => {
                for (axis, value) in ["x ", "y ", "z "].into_iter().zip(v.iter_mut()) {
                    ui.add(egui::DragValue::new(value).speed(0.01).prefix(axis));
                }
            }
        }
    });
}

/// The blend settings: blend-in, blend-out, the automatic blend-out and its lead, applied as one edit.
fn blend_row(ui: &mut egui::Ui, panel: &mut MontagePanel, described: &Value, edits: &mut Vec<Value>) {
    if panel.blend.as_ref().is_none_or(|(at, _)| *at != panel.reads) {
        panel.blend = Some((panel.reads, BlendForm::from_described(described)));
    }
    let Some((_, form)) = panel.blend.as_mut() else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label("Blend");
        ui.add(
            egui::DragValue::new(&mut form.blend_in)
                .speed(0.01)
                .range(0.0..=60.0)
                .prefix("in "),
        );
        ui.add(
            egui::DragValue::new(&mut form.blend_out)
                .speed(0.01)
                .range(0.0..=60.0)
                .prefix("out "),
        );
        ui.add(
            egui::DragValue::new(&mut form.blend_out_lead)
                .speed(0.01)
                .range(0.0..=60.0)
                .prefix("lead "),
        );
        ui.checkbox(&mut form.auto_blend_out, "auto blend-out");
        if ui.button("Apply blend").clicked()
            && let Some(edit) = form.edit(described)
        {
            edits.push(edit);
        }
    });
}

/// The slot-table editor: a skeleton's `.slots.ron` (read with `anim.montage_slots`) with add, set (group
/// and joint mask), rename and delete (through `anim.montage_slot_edit`; a rename rewrites the montages and
/// graphs that name the slot).
fn slot_table(ui: &mut egui::Ui, panel: &mut MontagePanel) {
    egui::CollapsingHeader::new("Slot table")
        .id_salt("montage-slot-table")
        .show(ui, |ui| {
            let table = &mut panel.slots;
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut table.path_input)
                        .hint_text("assets/anim/hero.slots.ron")
                        .desired_width(200.0),
                );
                if ui.button("Load slots").clicked() && !table.path_input.trim().is_empty() {
                    panel
                        .slot_requests
                        .push(SlotRequest::Load(table.path_input.trim().to_owned()));
                }
                ui.checkbox(&mut table.force, "force delete");
            });
            let Some(answer) = table.table.clone() else {
                ui.weak("Load a .slots.ron (a missing file starts from the default table).");
                return;
            };
            for slot in answer["slots"].as_array().into_iter().flatten() {
                let name = slot["name"].as_str().unwrap_or_default().to_owned();
                let users = answer["used_by"][&name].as_array().map_or(0, Vec::len)
                    + answer["used_by_graphs"][&name].as_array().map_or(0, Vec::len);
                ui.horizontal(|ui| {
                    ui.monospace(format!(
                        "{name} [{}] {} · {users} user(s)",
                        slot["group"].as_str().unwrap_or("default"),
                        if slot["mask"].is_null() {
                            "full body"
                        } else {
                            "masked"
                        },
                    ));
                    let to = table.rename.entry(name.clone()).or_insert_with(|| name.clone());
                    ui.add(egui::TextEdit::singleline(to).desired_width(90.0));
                    let to = to.trim().to_owned();
                    if ui.button("Rename slot").clicked() && !to.is_empty() && to != name {
                        panel.slot_requests.push(SlotRequest::Edit {
                            op: slot_rename_op(&name, &to),
                            force: false,
                        });
                    }
                    if ui.button("Delete slot").clicked() {
                        panel.slot_requests.push(SlotRequest::Edit {
                            op: slot_delete_op(&name),
                            force: table.force,
                        });
                    }
                });
                // The row's `set` op: its group and joint mask (no joints = full body).
                let form = table
                    .set_forms
                    .entry(name.clone())
                    .or_insert_with(|| SlotSetForm::of(slot));
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut form.group)
                            .hint_text("group")
                            .desired_width(70.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut form.joints)
                            .hint_text("joints (none = full body)")
                            .desired_width(160.0),
                    );
                    ui.checkbox(&mut form.include_children, "with children");
                    if ui.button("Set slot").clicked() {
                        panel.slot_requests.push(SlotRequest::Edit {
                            op: slot_set_op(&name, &form.group, &form.joints, form.include_children),
                            force: false,
                        });
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut table.new_name)
                        .hint_text("new slot")
                        .desired_width(90.0),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut table.new_group)
                        .hint_text("group")
                        .desired_width(70.0),
                );
                if ui.button("Add slot").clicked() && !table.new_name.trim().is_empty() {
                    panel.slot_requests.push(SlotRequest::Edit {
                        op: slot_add_op(table.new_name.trim(), &table.new_group),
                        force: false,
                    });
                    table.new_name.clear();
                }
            });
        });
}

// --------------------------------------------------------------------------------------- agent surface

fn with_panel<T>(
    world: &mut World,
    f: impl FnOnce(&mut MontagePanel, &mut World) -> Result<T, PanelError>,
) -> Result<T, RpcError> {
    let mut panel = world.remove_resource::<MontagePanel>().unwrap_or_default();
    let result = f(&mut panel, world);
    match &result {
        Ok(_) => panel.error = None,
        Err(e) => panel.error = Some(e.clone()),
    }
    world.insert_resource(panel);
    result.map_err(|e| e.rpc(HINT))
}

fn state(world: &World) -> Value {
    world
        .get_resource::<MontagePanel>()
        .map_or_else(|| MontagePanel::default().json(), MontagePanel::json)
}

fn panel_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let asset = str_param(params, "asset")?.to_owned();
    with_panel(world, |panel, world| panel.open(world, &asset))?;
    Ok(state(world))
}

fn panel_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    // Read the file again (an outside edit shows), keeping the last error line.
    if let Some(mut panel) = world.remove_resource::<MontagePanel>() {
        let result = if panel.asset.is_some() {
            panel.refresh(world)
        } else {
            Ok(())
        };
        panel.refresh_pose(world, false);
        world.insert_resource(panel);
        result.map_err(|e| e.rpc(HINT))?;
    }
    Ok(state(world))
}

fn panel_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let edit = params
        .get("edit")
        .filter(|e| e.is_object())
        .cloned()
        .ok_or_else(|| {
            PanelError::new("montage_bad_edit", "`edit` must be an object with an `op`").rpc(HINT)
        })?;
    let dry_run = opt_bool(params, "dry_run")?.unwrap_or(false);
    let report = with_panel(world, |panel, world| panel.edit(world, &edit, dry_run))?;
    let mut out = state(world);
    out["report"] = report;
    Ok(out)
}

fn panel_slot_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = params
        .get("op")
        .filter(|o| o.is_object())
        .cloned()
        .ok_or_else(|| {
            PanelError::new("montage_bad_edit", "`op` must be an object with an `op` name").rpc(HINT)
        })?;
    let force = opt_bool(params, "force")?.unwrap_or(false);
    let path = params.get("path").and_then(Value::as_str).map(str::to_owned);
    let report = with_panel(world, |panel, world| {
        if let Some(path) = &path {
            panel.load_slots(world, path)?;
        }
        panel.slot_edit(world, &op, force)
    })?;
    let mut out = state(world);
    out["report"] = report;
    Ok(out)
}

fn panel_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, world| {
        panel.set_preview(params)?;
        if let Some(rig) = params.get("rig") {
            panel.set_rig(world, rig)?;
        }
        panel.refresh_pose(world, false);
        Ok(())
    })?;
    Ok(state(world))
}

fn panel_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let picked = match params.get("kind").and_then(Value::as_str) {
        None => None,
        Some("segment") => Some(Picked::Segment {
            track: params["track"].as_u64().unwrap_or(0) as usize,
            segment: params["segment"].as_u64().unwrap_or(0) as usize,
        }),
        Some("section") => Some(Picked::Section(str_param(params, "name")?.to_owned())),
        Some("notify") => Some(Picked::Notify(str_param(params, "name")?.to_owned())),
        Some("warp") => Some(Picked::Warp(params["index"].as_u64().unwrap_or(0) as usize)),
        Some(other) => {
            return Err(
                PanelError::new("montage_panel_bad_value", format!("unknown kind `{other}`")).rpc(HINT),
            );
        }
    };
    with_panel(world, |panel, _| {
        if panel.asset.is_none() {
            return Err(err("no_asset", "open a montage first"));
        }
        panel.select(picked);
        Ok(())
    })?;
    Ok(state(world))
}

const fn param(
    name: &'static str,
    kind: &'static str,
    required: bool,
    description: &'static str,
) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const STATE: &str = "{ asset, montage: <anim.montage_describe>, layout: [{ track, segment, clip, at, length, known }], picked, \
    preview: { playhead, length, section, whole, chain: [{ section, start, end }], loops, active: [{ track, slot, segment, \
    clip, clip_time }] }, last_edit, journal_depth, slots: <anim.montage_slots> | null, error }";

const OPEN_PARAMS: &[ParamSchema] = &[param(
    "asset",
    "string",
    true,
    "Project-relative .montage.ron or .montagevar.ron.",
)];
const EDIT_PARAMS: &[ParamSchema] = &[
    param(
        "edit",
        "object",
        true,
        "An anim.montage_edit edit: { op: add_segment | move_segment | ... | undo, ... }; undo takes back the \
         panel's newest step (a slot-table edit puts its files back; a montage edit is the agent's undo).",
    ),
    param(
        "dry_run",
        "boolean",
        false,
        "Validate and report without writing.",
    ),
];
const SLOT_EDIT_PARAMS: &[ParamSchema] = &[
    param(
        "path",
        "string",
        false,
        "Load this project-relative .slots.ron into the slot-table editor first (absent: the loaded one).",
    ),
    param(
        "op",
        "object",
        true,
        "An anim.montage_slot_edit op: { op: add | set | rename | delete, ... }.",
    ),
    param(
        "force",
        "boolean",
        false,
        "delete: delete a slot that montages or graphs still name.",
    ),
];
const PREVIEW_PARAMS: &[ParamSchema] = &[
    param(
        "playhead",
        "number",
        false,
        "Montage time of the playhead (clamped to the timeline).",
    ),
    param(
        "section",
        "string",
        false,
        "The section the chain preview starts at; null = the first.",
    ),
    param(
        "whole",
        "boolean",
        false,
        "Preview the whole timeline order instead of one chain.",
    ),
    param(
        "rig",
        "number",
        false,
        "Pose-preview rig: an entity with a SkinnedMesh; null clears.",
    ),
];
const SELECT_PARAMS: &[ParamSchema] = &[
    param(
        "kind",
        "string",
        false,
        "segment | section | notify | warp; absent clears the selection.",
    ),
    param("track", "number", false, "segment: the track."),
    param("segment", "number", false, "segment: the index."),
    param("name", "string", false, "section, notify: the name."),
    param("index", "number", false, "warp: the index."),
];

/// The Montage timeline's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.montage_panel_open",
                tool: "montage_panel_open",
                description: "Open a montage in the Montage timeline (spec 34 M10).",
                params: OPEN_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.montage_panel_get",
                tool: "montage_panel_get",
                description: "The Montage timeline: the montage, its lanes as placed, the selection, and the \
                    preview (section chain, what each track plays at the playhead).",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            panel_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.montage_panel_edit",
                tool: "montage_panel_edit",
                description: "Apply one anim.montage_edit edit to the open montage (atomic, validated, undoable) and \
                    redraw the lanes.",
                params: EDIT_PARAMS,
                result: "STATE plus report: { moved, unlinked, saved, undo_depth }",
                mutating: true,
                permission: Permission::Assets,
            },
            panel_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.montage_panel_slot_edit",
                tool: "montage_panel_slot_edit",
                description: "Apply one anim.montage_slot_edit op to the slot-table editor's table as one step of the \
                    panel's undo journal (editor.montage_panel_edit { edit: { op: \"undo\" } } puts back every file \
                    it wrote, the table and the montages a rename rewrote), then read the table and the open montage \
                    again.",
                params: SLOT_EDIT_PARAMS,
                result: "STATE plus report: <anim.montage_slot_edit's answer>",
                mutating: true,
                permission: Permission::Assets,
            },
            panel_slot_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.montage_panel_preview",
                tool: "montage_panel_preview",
                description: "Move the playhead and choose the previewed section chain or the whole timeline order.",
                params: PREVIEW_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_preview as Handler,
        ),
        (
            MethodSchema {
                name: "editor.montage_panel_select",
                tool: "montage_panel_select",
                description: "Select a segment, section, notify or warp on the timeline (what the fields below the \
                    lanes edit).",
                params: SELECT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_select as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod authoring_tests;
