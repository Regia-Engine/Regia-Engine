// SPDX-License-Identifier: MPL-2.0
//! The smart-object definition editor (`docs/specs/39-smart-objects-v2.md` § 3, `docs/specs/19-editor.md`
//! § 2, ADR-7736): a `.smartobject.ron` definition's activity tags, requester rules and queries,
//! conditions, behavior payloads, slots and entrances as typed forms. Agent surface:
//! `editor.smart_object_def_state` and `editor.smart_object_def_edit` (`book/agents/smart-object-editor.md`).
//!
//! The panel reads and writes the file through `regia_ai::smart_object_def::SmartObjectDef::from_ron`
//! and `to_ron`, the parser and writer the runtime loader uses; the format is unchanged (version 1).
//! Every change is one [`SmartObjectDefOp`] applied as one [`SmartObjectDefEdit`] on the editor's undo
//! stack. The forms check only that an index exists: whether the definition is valid is the runtime
//! validator's answer (`SmartObjectDef::validate`), shown under the forms and required by Save, so the
//! editor never keeps a second copy of the format's rules. A definition nobody edited is never
//! rewritten: saving it leaves the file's bytes as they were. A file with comments loses them on the
//! first save (the canonical writer has none); the panel and the agent reply say so first.
//!
//! The slots and entrances of the open definition are also placed in the viewport
//! ([`crate::smart_object_handles`]).

use std::collections::BTreeMap;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ai::smart_object_def::{
    ActivityMergeDef, BehaviorDef, ConditionDef, EntranceDef, EntranceKindDef, OffsetDef, SlotDef,
    SmartObjectDef, UserRuleDef,
};
use regia_core::math::{EulerRot, Quat};
use regia_ecs::prelude::*;
use regia_framework::tags::TagQuery;
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack};
use crate::data_panels::{PanelError, confined, write_atomic};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/smart-object-editor.md";

/// The file suffix the editor opens and saves.
pub const SUFFIX: &str = ".smartobject.ron";

/// What the panel and the agent say while the open file still has its comments.
pub const COMMENTS_WARNING: &str = "This file has comments: saving writes the canonical form and drops them.";

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("smart_object_def_{code}"), message)
}

/// The part of the definition an op edits: the definition itself or one of its slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefScope {
    /// The definition-level field.
    Definition,
    /// The field of the slot at this index.
    Slot(usize),
}

/// One change to a definition: what a form row or `editor.smart_object_def_edit` asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum SmartObjectDefOp {
    /// The activity tags of the definition or a slot.
    Activity {
        /// Where.
        scope: DefScope,
        /// The tags, in order.
        tags: Vec<String>,
    },
    /// How the definition's activity tags combine with a slot's.
    ActivityMerge(ActivityMergeDef),
    /// Which requester rules apply.
    UserRule(UserRuleDef),
    /// The requester query of the definition or a slot (`None` passes everyone).
    UserQuery {
        /// Where.
        scope: DefScope,
        /// The query.
        query: Option<TagQuery>,
    },
    /// The conditions of the definition or a slot.
    Conditions {
        /// Where.
        scope: DefScope,
        /// The conditions, in order.
        conditions: Vec<ConditionDef>,
    },
    /// The behavior payloads of the definition or a slot.
    Behaviors {
        /// Where.
        scope: DefScope,
        /// The payloads, in order.
        behaviors: Vec<BehaviorDef>,
    },
    /// Appends a slot with this name and the format's defaults.
    AddSlot {
        /// Its name.
        name: String,
    },
    /// Removes the slot at an index.
    RemoveSlot(usize),
    /// Renames a slot.
    RenameSlot {
        /// The slot.
        slot: usize,
        /// The new name.
        name: String,
    },
    /// Whether a slot starts enabled.
    SlotEnabled {
        /// The slot.
        slot: usize,
        /// Enabled.
        enabled: bool,
    },
    /// The runtime tags a slot starts with.
    SlotInitialTags {
        /// The slot.
        slot: usize,
        /// The tags.
        tags: Vec<String>,
    },
    /// A slot's local offset in the object's space.
    SlotOffset {
        /// The slot.
        slot: usize,
        /// The offset.
        offset: OffsetDef,
    },
    /// Appends an entrance to a slot.
    AddEntrance {
        /// The slot.
        slot: usize,
        /// The entrance.
        entrance: EntranceDef,
    },
    /// Removes an entrance of a slot.
    RemoveEntrance {
        /// The slot.
        slot: usize,
        /// The entrance index.
        index: usize,
    },
    /// Replaces an entrance of a slot (offset, kind and rank).
    Entrance {
        /// The slot.
        slot: usize,
        /// The entrance index.
        index: usize,
        /// The entrance.
        entrance: EntranceDef,
    },
}

impl SmartObjectDefOp {
    /// Coalescing key: consecutive changes with one key (a drag of one value) are one undo step.
    fn key(&self) -> Option<String> {
        match self {
            Self::SlotOffset { slot, .. } => Some(format!("offset:{slot}")),
            Self::Entrance { slot, index, .. } => Some(format!("entrance:{slot}:{index}")),
            _ => None,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Activity { .. } => "activity",
            Self::ActivityMerge(_) => "activity merge",
            Self::UserRule(_) => "requester rule",
            Self::UserQuery { .. } => "requester query",
            Self::Conditions { .. } => "conditions",
            Self::Behaviors { .. } => "behaviors",
            Self::AddSlot { .. } => "add slot",
            Self::RemoveSlot(_) => "remove slot",
            Self::RenameSlot { .. } => "rename slot",
            Self::SlotEnabled { .. } => "slot enabled",
            Self::SlotInitialTags { .. } => "initial tags",
            Self::SlotOffset { .. } => "slot offset",
            Self::AddEntrance { .. } => "add entrance",
            Self::RemoveEntrance { .. } => "remove entrance",
            Self::Entrance { .. } => "entrance",
        }
    }
}

fn slot_mut(def: &mut SmartObjectDef, slot: usize) -> Result<&mut SlotDef, PanelError> {
    let count = def.slots.len();
    def.slots.get_mut(slot).ok_or_else(|| {
        err(
            "bad_index",
            format!("no slot {slot} (the definition has {count})"),
        )
    })
}

fn entrance_mut(def: &mut SmartObjectDef, slot: usize, index: usize) -> Result<&mut EntranceDef, PanelError> {
    let slot_def = slot_mut(def, slot)?;
    let count = slot_def.entrances.len();
    slot_def.entrances.get_mut(index).ok_or_else(|| {
        err(
            "bad_index",
            format!("slot {slot} has no entrance {index} (it has {count})"),
        )
    })
}

/// A new slot with `name` and the format's defaults (what a file that names only the slot loads as).
#[must_use]
pub fn default_slot(name: &str) -> SlotDef {
    SlotDef {
        name: name.to_owned(),
        offset: OffsetDef::default(),
        activity: Vec::new(),
        initial_tags: Vec::new(),
        enabled: true,
        user_query: None,
        conditions: Vec::new(),
        behaviors: Vec::new(),
        entrances: Vec::new(),
    }
}

/// The first `slot_N` name `def` does not use yet.
#[must_use]
pub fn unused_slot_name(def: &SmartObjectDef) -> String {
    (0..)
        .map(|n| format!("slot_{n}"))
        .find(|name| def.slots.iter().all(|s| &s.name != name))
        .unwrap_or_else(|| "slot".to_owned())
}

/// Applies `op` to `def`. Only the indices are checked here; the definition's rules are
/// `SmartObjectDef::validate`'s (shown by the panel, required by Save).
///
/// # Errors
///
/// `smart_object_def_bad_index` for a slot or entrance that does not exist.
pub fn apply_op(def: &mut SmartObjectDef, op: &SmartObjectDefOp) -> Result<(), PanelError> {
    match op {
        SmartObjectDefOp::Activity { scope, tags } => match scope {
            DefScope::Definition => def.activity.clone_from(tags),
            DefScope::Slot(i) => slot_mut(def, *i)?.activity.clone_from(tags),
        },
        SmartObjectDefOp::ActivityMerge(merge) => def.activity_merge = *merge,
        SmartObjectDefOp::UserRule(rule) => def.user_rule = *rule,
        SmartObjectDefOp::UserQuery { scope, query } => match scope {
            DefScope::Definition => def.user_query.clone_from(query),
            DefScope::Slot(i) => slot_mut(def, *i)?.user_query.clone_from(query),
        },
        SmartObjectDefOp::Conditions { scope, conditions } => match scope {
            DefScope::Definition => def.conditions.clone_from(conditions),
            DefScope::Slot(i) => slot_mut(def, *i)?.conditions.clone_from(conditions),
        },
        SmartObjectDefOp::Behaviors { scope, behaviors } => match scope {
            DefScope::Definition => def.behaviors.clone_from(behaviors),
            DefScope::Slot(i) => slot_mut(def, *i)?.behaviors.clone_from(behaviors),
        },
        SmartObjectDefOp::AddSlot { name } => def.slots.push(default_slot(name)),
        SmartObjectDefOp::RemoveSlot(i) => {
            slot_mut(def, *i)?;
            def.slots.remove(*i);
        }
        SmartObjectDefOp::RenameSlot { slot, name } => slot_mut(def, *slot)?.name.clone_from(name),
        SmartObjectDefOp::SlotEnabled { slot, enabled } => slot_mut(def, *slot)?.enabled = *enabled,
        SmartObjectDefOp::SlotInitialTags { slot, tags } => {
            slot_mut(def, *slot)?.initial_tags.clone_from(tags);
        }
        SmartObjectDefOp::SlotOffset { slot, offset } => slot_mut(def, *slot)?.offset = *offset,
        SmartObjectDefOp::AddEntrance { slot, entrance } => {
            slot_mut(def, *slot)?.entrances.push(entrance.clone());
        }
        SmartObjectDefOp::RemoveEntrance { slot, index } => {
            entrance_mut(def, *slot, *index)?;
            slot_mut(def, *slot)?.entrances.remove(*index);
        }
        SmartObjectDefOp::Entrance {
            slot,
            index,
            entrance,
        } => *entrance_mut(def, *slot, *index)? = entrance.clone(),
    }
    Ok(())
}

/// The open definition.
#[derive(Clone, Debug)]
pub struct SmartObjectDefDoc {
    /// Project-relative path of the `.smartobject.ron`.
    pub path: String,
    /// The file's text when it was opened or last saved (byte-exact).
    pub original: String,
    /// The definition as that text parses.
    pub saved: SmartObjectDef,
    /// The definition being edited.
    pub def: SmartObjectDef,
}

impl SmartObjectDefDoc {
    /// Whether the edited definition differs from the file.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.def != self.saved
    }

    /// The runtime validator's message for the edited definition (`None` when it is valid).
    #[must_use]
    pub fn validation(&self) -> Option<String> {
        self.def.validate().err().map(|e| e.to_string())
    }

    /// [`COMMENTS_WARNING`] while the file on disk still has comments a save would drop.
    #[must_use]
    pub fn comments_warning(&self) -> Option<&'static str> {
        crate::abilities_panel::form::has_comments(&self.original).then_some(COMMENTS_WARNING)
    }
}

/// The definition editor's state (a resource of the edit world).
#[derive(Resource, Debug, Default)]
pub struct SmartObjectDefPanel {
    doc: Option<SmartObjectDefDoc>,
    /// The entity the slot handles show on besides the objects that name the open file.
    pub preview: Option<Entity>,
    /// The viewport's slot handles are off.
    pub handles_off: bool,
    last_error: Option<String>,
    /// UI scratch: the path box, the selected slot and the text rows being typed.
    path_input: String,
    selected_slot: usize,
    scratch: BTreeMap<String, String>,
    /// The egui frame whose clicks were applied: a second UI pass in that frame applies nothing.
    applied_frame: Option<u64>,
}

impl SmartObjectDefPanel {
    /// The open definition.
    #[must_use]
    pub fn doc(&self) -> Option<&SmartObjectDefDoc> {
        self.doc.as_ref()
    }

    /// The open definition, editable by the handle drags (not an undo step by itself).
    pub(crate) fn doc_mut(&mut self) -> Option<&mut SmartObjectDefDoc> {
        self.doc.as_mut()
    }
}

/// One reversible definition change (one undo step).
pub struct SmartObjectDefEdit {
    /// The definition before.
    pub before: SmartObjectDef,
    /// The definition after.
    pub after: SmartObjectDef,
    key: Option<String>,
    label: &'static str,
}

impl SmartObjectDefEdit {
    /// An edit from `before` to `after` that never coalesces (a whole viewport drag).
    #[must_use]
    pub fn single(before: SmartObjectDef, after: SmartObjectDef, label: &'static str) -> Self {
        Self {
            before,
            after,
            key: None,
            label,
        }
    }

    fn set(world: &mut World, def: &SmartObjectDef) {
        let mut panel = world.remove_resource::<SmartObjectDefPanel>().unwrap_or_default();
        if let Some(doc) = panel.doc.as_mut() {
            doc.def = def.clone();
        }
        world.insert_resource(panel);
    }
}

impl EditorCommand for SmartObjectDefEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<SmartObjectDefEdit>() else {
            return false;
        };
        if self.key.is_some() && self.key == next.key {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    // The definition is a file of its own, not the level.
    fn changes_level_state(&self) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Smart object definition: {}", self.label)
    }
}

/// Opens the definition at the project-relative `path` (replacing the open one).
///
/// # Errors
///
/// `smart_object_def_no_project`, `smart_object_def_bad_path`, `smart_object_def_io_failed` or
/// `smart_object_def_parse_failed` (the loader's message, which names `bad_definition` or
/// `unsupported_version`).
pub fn open_def(world: &mut World, path: &str) -> Result<(), PanelError> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| err("no_project", "no project is open; open one first"))?;
    let full = confined(&root, path, &[SUFFIX], "smart_object_def")?;
    let text =
        std::fs::read_to_string(&full).map_err(|e| err("io_failed", format!("{}: {e}", full.display())))?;
    let def = SmartObjectDef::from_ron(&text)
        .map_err(|e| err("parse_failed", format!("{path}: {e} ({})", e.reason())))?;
    let mut panel = world.remove_resource::<SmartObjectDefPanel>().unwrap_or_default();
    panel.path_input = path.to_owned();
    panel.selected_slot = 0;
    panel.scratch.clear();
    panel.doc = Some(SmartObjectDefDoc {
        path: path.to_owned(),
        original: text,
        saved: def.clone(),
        def,
    });
    panel.last_error = None;
    world.insert_resource(panel);
    Ok(())
}

/// Applies `op` to the open definition as one undo step. `Ok(false)` when it changed nothing.
///
/// # Errors
///
/// `smart_object_def_no_definition` when none is open, else [`apply_op`]'s errors.
pub fn edit_def(world: &mut World, op: &SmartObjectDefOp) -> Result<bool, PanelError> {
    let before = world
        .get_resource::<SmartObjectDefPanel>()
        .and_then(|p| p.doc.as_ref())
        .map(|d| d.def.clone())
        .ok_or_else(|| err("no_definition", "no definition is open; pass `path`"))?;
    let mut after = before.clone();
    apply_op(&mut after, op)?;
    if after == before {
        return Ok(false);
    }
    let mut command = SmartObjectDefEdit {
        before,
        after,
        key: op.key(),
        label: op.label(),
    };
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    Ok(true)
}

/// Saves the open definition. An unedited one is left alone (the file keeps its bytes); an edited
/// one must pass the runtime validator and is written with `SmartObjectDef::to_ron`, atomically.
/// Returns the bytes written (0 when nothing was written).
///
/// # Errors
///
/// `smart_object_def_no_definition`, `smart_object_def_no_project`, `smart_object_def_invalid`
/// (the validator's message), `smart_object_def_serialize_failed` or `smart_object_def_io_failed`.
pub fn save_def(world: &mut World) -> Result<usize, PanelError> {
    let root =
        crate::mesh_source::project_root(world).ok_or_else(|| err("no_project", "no project is open"))?;
    let mut panel = world.remove_resource::<SmartObjectDefPanel>().unwrap_or_default();
    let result = (|| -> Result<_, PanelError> {
        let doc = panel
            .doc
            .as_mut()
            .ok_or_else(|| err("no_definition", "no definition is open"))?;
        if !doc.dirty() {
            return Ok(0);
        }
        doc.def.validate().map_err(|e| err("invalid", e.to_string()))?;
        let text = doc
            .def
            .to_ron()
            .map_err(|e| err("serialize_failed", e.to_string()))?;
        let full = confined(&root, &doc.path, &[SUFFIX], "smart_object_def")?;
        write_atomic(&full, &text, "smart_object_def")?;
        let written = text.len();
        doc.original = text;
        doc.saved = doc.def.clone();
        Ok(written)
    })();
    world.insert_resource(panel);
    result
}

// --------------------------------------------------------------------------------------- names

fn merge_name(m: ActivityMergeDef) -> &'static str {
    match m {
        ActivityMergeDef::Union => "union",
        ActivityMergeDef::SlotFirst => "slot_first",
    }
}

fn rule_name(r: UserRuleDef) -> &'static str {
    match r {
        UserRuleDef::DefinitionAndSlot => "definition_and_slot",
        UserRuleDef::SlotOnly => "slot_only",
        UserRuleDef::SlotElseDefinition => "slot_else_definition",
    }
}

fn kind_name(k: EntranceKindDef) -> &'static str {
    match k {
        EntranceKindDef::Enter => "enter",
        EntranceKindDef::Exit => "exit",
        EntranceKindDef::Both => "both",
    }
}

const MERGES: [ActivityMergeDef; 2] = [ActivityMergeDef::Union, ActivityMergeDef::SlotFirst];
const RULES: [UserRuleDef; 3] = [
    UserRuleDef::DefinitionAndSlot,
    UserRuleDef::SlotOnly,
    UserRuleDef::SlotElseDefinition,
];
const KINDS: [EntranceKindDef; 3] = [
    EntranceKindDef::Enter,
    EntranceKindDef::Exit,
    EntranceKindDef::Both,
];

/// A requester query as RON text (`Has("Role.Guard")`, `AnyOf([...])`); empty text is `None`.
///
/// # Errors
///
/// `smart_object_def_bad_value` when the text is not a tag query.
pub fn parse_query(text: &str) -> Result<Option<TagQuery>, PanelError> {
    let text = text.trim();
    if text.is_empty() || text == "None" {
        return Ok(None);
    }
    ron::from_str::<TagQuery>(text)
        .map(Some)
        .map_err(|e| err("bad_value", format!("not a tag query: {e}")))
}

fn query_text(query: Option<&TagQuery>) -> String {
    query
        .and_then(|q| ron::ser::to_string(q).ok())
        .unwrap_or_default()
}

/// Tags typed as a comma-separated list.
fn tags_of(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Condition parameters typed as `key=value; key=value`.
fn params_of(text: &str) -> BTreeMap<String, String> {
    text.split(';')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_owned(), v.trim().to_owned()))
        })
        .collect()
}

fn params_text(params: &BTreeMap<String, String>) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Yaw, pitch and roll in degrees (YXZ) of an xyzw rotation.
fn euler_degrees(rotation: (f32, f32, f32, f32)) -> [f32; 3] {
    let (x, y, z, w) = rotation;
    let (yaw, pitch, roll) = Quat::from_xyzw(x, y, z, w).normalize().to_euler(EulerRot::YXZ);
    [yaw.to_degrees(), pitch.to_degrees(), roll.to_degrees()]
}

fn rotation_of(degrees: [f32; 3]) -> (f32, f32, f32, f32) {
    let q = Quat::from_euler(
        EulerRot::YXZ,
        degrees[0].to_radians(),
        degrees[1].to_radians(),
        degrees[2].to_radians(),
    );
    (q.x, q.y, q.z, q.w)
}

// --------------------------------------------------------------------------------------- panel

/// A one-line text row bound to `current`: typing edits the scratch copy, Enter or leaving the field
/// returns the text to commit (only when it differs from `current`).
fn text_row(
    ui: &mut egui::Ui,
    scratch: &mut BTreeMap<String, String>,
    id: &str,
    current: &str,
    hint: &str,
    width: f32,
) -> Option<String> {
    let entry = scratch.entry(id.to_owned()).or_insert_with(|| current.to_owned());
    let response = ui.add(
        egui::TextEdit::singleline(entry)
            .hint_text(hint)
            .desired_width(width),
    );
    if response.has_focus() {
        return None;
    }
    let text = scratch.remove(id).unwrap_or_default();
    (response.lost_focus() && text != current).then_some(text)
}

fn offset_row(ui: &mut egui::Ui, offset: &OffsetDef) -> Option<OffsetDef> {
    let (mut x, mut y, mut z) = offset.translation;
    let mut euler = euler_degrees(offset.rotation);
    let mut moved = false;
    let mut turned = false;
    ui.horizontal(|ui| {
        ui.label("position");
        for v in [&mut x, &mut y, &mut z] {
            moved |= ui.add(egui::DragValue::new(v).speed(0.01)).changed();
        }
        ui.label("yaw/pitch/roll");
        for v in &mut euler {
            turned |= ui.add(egui::DragValue::new(v).speed(0.5).suffix("°")).changed();
        }
    });
    (moved || turned).then(|| OffsetDef {
        translation: (x, y, z),
        rotation: if turned {
            rotation_of(euler)
        } else {
            offset.rotation
        },
    })
}

/// The conditions or behaviors of one scope as editable rows.
fn conditions_rows(
    ui: &mut egui::Ui,
    scratch: &mut BTreeMap<String, String>,
    scope: DefScope,
    conditions: &[ConditionDef],
    ops: &mut Vec<SmartObjectDefOp>,
) {
    let tag = scope_tag(scope);
    for (i, c) in conditions.iter().enumerate() {
        ui.horizontal(|ui| {
            let path = text_row(
                ui,
                scratch,
                &format!("{tag}-cond-path-{i}"),
                &c.path,
                "my_game::Condition",
                180.0,
            );
            let params = text_row(
                ui,
                scratch,
                &format!("{tag}-cond-params-{i}"),
                &params_text(&c.params),
                "key=value; key=value",
                180.0,
            );
            let remove = ui.small_button("Remove condition").clicked();
            if path.is_some() || params.is_some() || remove {
                let mut next = conditions.to_vec();
                if remove {
                    next.remove(i);
                } else {
                    if let Some(path) = path {
                        next[i].path = path.trim().to_owned();
                    }
                    if let Some(params) = params {
                        next[i].params = params_of(&params);
                    }
                }
                ops.push(SmartObjectDefOp::Conditions {
                    scope,
                    conditions: next,
                });
            }
        });
    }
    if ui.small_button("Add condition").clicked() {
        let mut next = conditions.to_vec();
        next.push(ConditionDef {
            path: String::new(),
            params: BTreeMap::new(),
        });
        ops.push(SmartObjectDefOp::Conditions {
            scope,
            conditions: next,
        });
    }
}

fn behaviors_rows(
    ui: &mut egui::Ui,
    scratch: &mut BTreeMap<String, String>,
    scope: DefScope,
    behaviors: &[BehaviorDef],
    ops: &mut Vec<SmartObjectDefOp>,
) {
    let tag = scope_tag(scope);
    for (i, b) in behaviors.iter().enumerate() {
        ui.horizontal(|ui| {
            let family = text_row(
                ui,
                scratch,
                &format!("{tag}-beh-family-{i}"),
                &b.family,
                "family",
                120.0,
            );
            let data = text_row(
                ui,
                scratch,
                &format!("{tag}-beh-data-{i}"),
                &b.data,
                "(pose: \"relaxed\")",
                220.0,
            );
            let remove = ui.small_button("Remove behavior").clicked();
            if family.is_some() || data.is_some() || remove {
                let mut next = behaviors.to_vec();
                if remove {
                    next.remove(i);
                } else {
                    if let Some(family) = family {
                        next[i].family = family.trim().to_owned();
                    }
                    if let Some(data) = data {
                        next[i].data = data;
                    }
                }
                ops.push(SmartObjectDefOp::Behaviors {
                    scope,
                    behaviors: next,
                });
            }
        });
    }
    if ui.small_button("Add behavior").clicked() {
        let mut next = behaviors.to_vec();
        next.push(BehaviorDef {
            family: "family".to_owned(),
            data: String::new(),
        });
        ops.push(SmartObjectDefOp::Behaviors {
            scope,
            behaviors: next,
        });
    }
}

fn scope_tag(scope: DefScope) -> String {
    match scope {
        DefScope::Definition => "def".to_owned(),
        DefScope::Slot(i) => format!("slot{i}"),
    }
}

/// The shared rows of the definition and of a slot: activity, requester query, conditions, behaviors.
fn shared_rows(
    ui: &mut egui::Ui,
    panel: &mut SmartObjectDefPanel,
    scope: DefScope,
    fields: (&[String], Option<&TagQuery>, &[ConditionDef], &[BehaviorDef]),
    ops: &mut Vec<SmartObjectDefOp>,
) {
    let (activity, query, conditions, behaviors) = fields;
    let tag = scope_tag(scope);
    ui.horizontal(|ui| {
        ui.label("activity");
        if let Some(text) = text_row(
            ui,
            &mut panel.scratch,
            &format!("{tag}-activity"),
            &activity.join(", "),
            "Activity.Sit, Activity.Rest",
            260.0,
        ) {
            ops.push(SmartObjectDefOp::Activity {
                scope,
                tags: tags_of(&text),
            });
        }
    });
    ui.horizontal(|ui| {
        ui.label("requester query");
        if let Some(text) = text_row(
            ui,
            &mut panel.scratch,
            &format!("{tag}-query"),
            &query_text(query),
            "AnyOf([Has(\"Role.Guard\")])",
            260.0,
        ) {
            match parse_query(&text) {
                Ok(query) => ops.push(SmartObjectDefOp::UserQuery { scope, query }),
                Err(e) => panel.last_error = Some(e.message),
            }
        }
    });
    ui.label("conditions");
    conditions_rows(ui, &mut panel.scratch, scope, conditions, ops);
    ui.label("behaviors");
    behaviors_rows(ui, &mut panel.scratch, scope, behaviors, ops);
}

fn slot_form(
    ui: &mut egui::Ui,
    panel: &mut SmartObjectDefPanel,
    i: usize,
    slot: &SlotDef,
    ops: &mut Vec<SmartObjectDefOp>,
) {
    ui.horizontal(|ui| {
        ui.label("name");
        if let Some(name) = text_row(
            ui,
            &mut panel.scratch,
            &format!("slot{i}-name"),
            &slot.name,
            "seat",
            140.0,
        ) {
            ops.push(SmartObjectDefOp::RenameSlot {
                slot: i,
                name: name.trim().to_owned(),
            });
        }
        let mut enabled = slot.enabled;
        if ui.checkbox(&mut enabled, "starts enabled").changed() {
            ops.push(SmartObjectDefOp::SlotEnabled { slot: i, enabled });
        }
        if ui.small_button("Remove slot").clicked() {
            ops.push(SmartObjectDefOp::RemoveSlot(i));
        }
    });
    if let Some(offset) = offset_row(ui, &slot.offset) {
        ops.push(SmartObjectDefOp::SlotOffset { slot: i, offset });
    }
    ui.horizontal(|ui| {
        ui.label("initial tags");
        if let Some(text) = text_row(
            ui,
            &mut panel.scratch,
            &format!("slot{i}-initial"),
            &slot.initial_tags.join(", "),
            "State.Clean",
            260.0,
        ) {
            ops.push(SmartObjectDefOp::SlotInitialTags {
                slot: i,
                tags: tags_of(&text),
            });
        }
    });
    shared_rows(
        ui,
        panel,
        DefScope::Slot(i),
        (
            &slot.activity,
            slot.user_query.as_ref(),
            &slot.conditions,
            &slot.behaviors,
        ),
        ops,
    );
    ui.label("entrances");
    for (j, entrance) in slot.entrances.iter().enumerate() {
        ui.push_id(("entrance", i, j), |ui| {
            ui.horizontal(|ui| {
                ui.weak(format!("#{j}"));
                let mut kind = entrance.kind;
                egui::ComboBox::from_id_salt("entrance-kind")
                    .selected_text(kind_name(kind))
                    .show_ui(ui, |ui| {
                        for k in KINDS {
                            ui.selectable_value(&mut kind, k, kind_name(k));
                        }
                    });
                let mut rank = i32::from(entrance.rank);
                let ranked = ui
                    .add(egui::DragValue::new(&mut rank).range(-128..=127).prefix("rank "))
                    .changed();
                if kind != entrance.kind || ranked {
                    ops.push(SmartObjectDefOp::Entrance {
                        slot: i,
                        index: j,
                        entrance: EntranceDef {
                            kind,
                            rank: i8::try_from(rank).unwrap_or(entrance.rank),
                            ..entrance.clone()
                        },
                    });
                }
                if ui.small_button("Remove entrance").clicked() {
                    ops.push(SmartObjectDefOp::RemoveEntrance { slot: i, index: j });
                }
            });
            if let Some(offset) = offset_row(ui, &entrance.offset) {
                ops.push(SmartObjectDefOp::Entrance {
                    slot: i,
                    index: j,
                    entrance: EntranceDef {
                        offset,
                        ..entrance.clone()
                    },
                });
            }
        });
    }
    if ui.small_button("Add entrance").clicked() {
        ops.push(SmartObjectDefOp::AddEntrance {
            slot: i,
            entrance: EntranceDef {
                offset: OffsetDef {
                    translation: (0.0, 0.0, 1.0),
                    ..OffsetDef::default()
                },
                kind: EntranceKindDef::Both,
                rank: 0,
            },
        });
    }
}

/// Ctrl+S (Cmd+S on macOS) saves while the pointer is over the panel and no text field has focus.
/// The key is consumed; a second UI pass in the same frame finds nothing to save (the file is
/// clean) and writes nothing.
fn save_shortcut(ui: &egui::Ui) -> bool {
    ui.rect_contains_pointer(ui.max_rect())
        && !ui.ctx().text_edit_focused()
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S))
}

/// Draws the panel.
pub fn smart_object_def_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<SmartObjectDefPanel>().unwrap_or_default();
    let applied_frame = panel.applied_frame;
    let mut open: Option<String> = None;
    let mut ops: Vec<SmartObjectDefOp> = Vec::new();
    let mut do_save = save_shortcut(ui);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut panel.path_input).hint_text("assets/props/bench.smartobject.ron"),
        );
        if ui.button("Open").clicked() {
            open = Some(panel.path_input.trim().to_owned());
        }
    });
    if let Some(doc) = panel.doc.clone() {
        let def = &doc.def;
        ui.horizontal(|ui| {
            ui.strong(&doc.path);
            if doc.dirty() {
                ui.weak("(edited)");
            }
            if ui.add_enabled(doc.dirty(), egui::Button::new("Save")).clicked() {
                do_save = true;
            }
            let mut handles = !panel.handles_off;
            if ui.checkbox(&mut handles, "Slot handles").changed() {
                panel.handles_off = !handles;
            }
        });
        if let Some(warning) = doc.comments_warning() {
            ui.colored_label(egui::Color32::from_rgb(230, 180, 60), warning);
        }
        match doc.validation() {
            Some(message) => {
                ui.colored_label(egui::Color32::from_rgb(230, 90, 90), message);
            }
            None => {
                ui.weak("valid");
            }
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("smart-object-def")
            .show(ui, |ui| {
                ui.strong("Definition");
                ui.horizontal(|ui| {
                    let mut merge = def.activity_merge;
                    egui::ComboBox::from_id_salt("so-def-merge")
                        .selected_text(format!("activity merge: {}", merge_name(merge)))
                        .show_ui(ui, |ui| {
                            for m in MERGES {
                                ui.selectable_value(&mut merge, m, merge_name(m));
                            }
                        });
                    if merge != def.activity_merge {
                        ops.push(SmartObjectDefOp::ActivityMerge(merge));
                    }
                    let mut rule = def.user_rule;
                    egui::ComboBox::from_id_salt("so-def-rule")
                        .selected_text(format!("requester rule: {}", rule_name(rule)))
                        .show_ui(ui, |ui| {
                            for r in RULES {
                                ui.selectable_value(&mut rule, r, rule_name(r));
                            }
                        });
                    if rule != def.user_rule {
                        ops.push(SmartObjectDefOp::UserRule(rule));
                    }
                });
                shared_rows(
                    ui,
                    &mut panel,
                    DefScope::Definition,
                    (
                        &def.activity,
                        def.user_query.as_ref(),
                        &def.conditions,
                        &def.behaviors,
                    ),
                    &mut ops,
                );
                ui.separator();
                ui.horizontal(|ui| {
                    ui.strong("Slots");
                    if ui.small_button("Add slot").clicked() {
                        ops.push(SmartObjectDefOp::AddSlot {
                            name: unused_slot_name(def),
                        });
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    for (i, slot) in def.slots.iter().enumerate() {
                        ui.selectable_value(&mut panel.selected_slot, i, &slot.name);
                    }
                });
                let selected = panel.selected_slot.min(def.slots.len().saturating_sub(1));
                if let Some(slot) = def.slots.get(selected) {
                    ui.push_id(("slot", selected), |ui| {
                        slot_form(ui, &mut panel, selected, slot, &mut ops)
                    });
                }
            });
    } else {
        ui.weak("Open a .smartobject.ron from the project to edit its slots, or create one with Content ▸ New ▸ Smart object.");
    }
    if let Some(e) = &panel.last_error {
        ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e);
    }
    world.insert_resource(panel);

    // egui may run the panel twice in one frame: the second pass must not add a second slot.
    let frame = ui.ctx().cumulative_frame_nr();
    if applied_frame == Some(frame) {
        return;
    }
    let mut result: Result<(), PanelError> = Ok(());
    if let Some(path) = open {
        result = open_def(world, &path);
    }
    for op in &ops {
        result = result.and_then(|()| edit_def(world, op).map(|_| ()));
    }
    if do_save
        && world
            .get_resource::<SmartObjectDefPanel>()
            .is_some_and(|p| p.doc.is_some())
    {
        result = result.and_then(|()| save_def(world).map(|_| ()));
    }
    let mut panel = world.remove_resource::<SmartObjectDefPanel>().unwrap_or_default();
    panel.applied_frame = Some(frame);
    match result {
        Err(e) => panel.last_error = Some(e.message),
        Ok(()) if !ops.is_empty() || do_save => panel.last_error = None,
        Ok(()) => {}
    }
    if let Some(doc) = &panel.doc {
        panel.selected_slot = panel.selected_slot.min(doc.def.slots.len().saturating_sub(1));
    }
    world.insert_resource(panel);
}

// --------------------------------------------------------------------------------------- agent surface

/// The open definition as the agent sees it.
pub(crate) fn doc_json(world: &World) -> Value {
    let panel = world.get_resource::<SmartObjectDefPanel>();
    let handles = json!({
        "enabled": panel.is_none_or(|p| !p.handles_off),
        "preview": panel.and_then(|p| p.preview).map(Entity::to_bits),
    });
    let Some(doc) = panel.and_then(|p| p.doc.as_ref()) else {
        return json!({ "open": false, "handles": handles });
    };
    let def = &doc.def;
    let validation = doc.validation();
    json!({
        "open": true,
        "path": doc.path,
        "dirty": doc.dirty(),
        "valid": validation.is_none(),
        "validation": validation,
        "comments_warning": doc.comments_warning(),
        "activity": def.activity,
        "activity_merge": merge_name(def.activity_merge),
        "user_rule": rule_name(def.user_rule),
        "user_query": def.user_query.as_ref().map(|q| query_text(Some(q))),
        "conditions": def.conditions.iter().map(|c| json!({ "path": c.path, "params": c.params })).collect::<Vec<_>>(),
        "behaviors": def.behaviors.iter().map(|b| json!({ "family": b.family, "data": b.data })).collect::<Vec<_>>(),
        "slots": def.slots.iter().map(slot_json).collect::<Vec<_>>(),
        "handles": handles,
    })
}

fn offset_json(offset: &OffsetDef) -> Value {
    let (x, y, z) = offset.translation;
    let (qx, qy, qz, qw) = offset.rotation;
    json!({ "translation": [x, y, z], "rotation": [qx, qy, qz, qw] })
}

fn slot_json(slot: &SlotDef) -> Value {
    json!({
        "name": slot.name,
        "offset": offset_json(&slot.offset),
        "enabled": slot.enabled,
        "activity": slot.activity,
        "initial_tags": slot.initial_tags,
        "user_query": slot.user_query.as_ref().map(|q| query_text(Some(q))),
        "conditions": slot.conditions.iter().map(|c| json!({ "path": c.path, "params": c.params })).collect::<Vec<_>>(),
        "behaviors": slot.behaviors.iter().map(|b| json!({ "family": b.family, "data": b.data })).collect::<Vec<_>>(),
        "entrances": slot.entrances.iter().map(|e| json!({
            "offset": offset_json(&e.offset),
            "kind": kind_name(e.kind),
            "rank": e.rank,
        })).collect::<Vec<_>>(),
    })
}

fn bad(message: impl Into<String>) -> RpcError {
    err("bad_value", message).rpc(HINT)
}

fn current_def(world: &World) -> Result<SmartObjectDef, RpcError> {
    world
        .get_resource::<SmartObjectDefPanel>()
        .and_then(|p| p.doc.as_ref())
        .map(|d| d.def.clone())
        .ok_or_else(|| err("no_definition", "no definition is open; pass `path`").rpc(HINT))
}

/// The slot index a `slot` parameter names: an index, or a slot name.
fn slot_param(def: &SmartObjectDef, params: &Value) -> Result<Option<usize>, RpcError> {
    match params.get("slot") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => def
            .slots
            .iter()
            .position(|s| &s.name == name)
            .map(Some)
            .ok_or_else(|| err("bad_index", format!("no slot named {name:?}")).rpc(HINT)),
        Some(v) => v
            .as_u64()
            .map(|i| Some(i as usize))
            .ok_or_else(|| bad("`slot` must be an index or a slot name")),
    }
}

fn required_slot(def: &SmartObjectDef, params: &Value) -> Result<usize, RpcError> {
    slot_param(def, params)?.ok_or_else(|| bad("`slot` (an index or a slot name) is required"))
}

fn scope_param(def: &SmartObjectDef, params: &Value) -> Result<DefScope, RpcError> {
    Ok(slot_param(def, params)?.map_or(DefScope::Definition, DefScope::Slot))
}

fn strings(params: &Value, key: &str) -> Result<Option<Vec<String>>, RpcError> {
    match params.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| bad(format!("`{key}` must be an array of strings")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(bad(format!("`{key}` must be an array of strings"))),
    }
}

fn floats<const N: usize>(params: &Value, key: &str) -> Result<Option<[f32; N]>, RpcError> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .filter(|a| a.len() == N)
        .ok_or_else(|| bad(format!("`{key}` must be an array of {N} numbers")))?;
    let mut out = [0.0_f32; N];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item
            .as_f64()
            .filter(|v| v.is_finite())
            .ok_or_else(|| bad(format!("`{key}` must be an array of {N} finite numbers")))?
            as f32;
    }
    Ok(Some(out))
}

/// `translation` / `rotation` over `base`.
fn offset_param(params: &Value, base: OffsetDef) -> Result<Option<OffsetDef>, RpcError> {
    let translation = floats::<3>(params, "translation")?;
    let rotation = floats::<4>(params, "rotation")?;
    if translation.is_none() && rotation.is_none() {
        return Ok(None);
    }
    Ok(Some(OffsetDef {
        translation: translation.map_or(base.translation, |[x, y, z]| (x, y, z)),
        rotation: rotation.map_or(base.rotation, |[x, y, z, w]| (x, y, z, w)),
    }))
}

fn kind_param(params: &Value) -> Result<Option<EntranceKindDef>, RpcError> {
    match params.get("kind").and_then(Value::as_str) {
        None => Ok(None),
        Some(name) => KINDS
            .into_iter()
            .find(|k| kind_name(*k) == name)
            .map(Some)
            .ok_or_else(|| bad(format!("`kind` must be enter, exit or both, not {name:?}"))),
    }
}

fn rank_param(params: &Value) -> Result<Option<i8>, RpcError> {
    match params.get("rank") {
        None => Ok(None),
        Some(v) => v
            .as_i64()
            .and_then(|r| i8::try_from(r).ok())
            .map(Some)
            .ok_or_else(|| bad("`rank` must be an integer from -128 to 127")),
    }
}

fn conditions_param(params: &Value) -> Result<Vec<ConditionDef>, RpcError> {
    let items = params
        .get("conditions")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("`conditions` (an array of { path, params? }) is required"))?;
    items
        .iter()
        .map(|item| {
            let path = item
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("each condition needs a string `path`"))?;
            let mut map = BTreeMap::new();
            if let Some(p) = item.get("params") {
                let obj = p
                    .as_object()
                    .ok_or_else(|| bad("a condition's `params` must be an object of strings"))?;
                for (k, v) in obj {
                    let v = v
                        .as_str()
                        .ok_or_else(|| bad("a condition's `params` must be an object of strings"))?;
                    map.insert(k.clone(), v.to_owned());
                }
            }
            Ok(ConditionDef {
                path: path.to_owned(),
                params: map,
            })
        })
        .collect()
}

fn behaviors_param(params: &Value) -> Result<Vec<BehaviorDef>, RpcError> {
    let items = params
        .get("behaviors")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("`behaviors` (an array of { family, data? }) is required"))?;
    items
        .iter()
        .map(|item| {
            let family = item
                .get("family")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("each behavior needs a string `family`"))?;
            let data = match item.get("data") {
                None => String::new(),
                Some(v) => v
                    .as_str()
                    .ok_or_else(|| bad("a behavior's `data` must be RON text"))?
                    .to_owned(),
            };
            Ok(BehaviorDef {
                family: family.to_owned(),
                data,
            })
        })
        .collect()
}

/// The ops one `editor.smart_object_def_edit` call asks for, in order.
fn ops_of(world: &World, op: &str, params: &Value) -> Result<Vec<SmartObjectDefOp>, RpcError> {
    let def = current_def(world)?;
    let mut out = Vec::new();
    match op {
        "set_activity" => {
            let tags =
                strings(params, "tags")?.ok_or_else(|| bad("`tags` (an array of strings) is required"))?;
            out.push(SmartObjectDefOp::Activity {
                scope: scope_param(&def, params)?,
                tags,
            });
        }
        "set_rules" => {
            if let Some(name) = params.get("activity_merge").and_then(Value::as_str) {
                let merge = MERGES
                    .into_iter()
                    .find(|m| merge_name(*m) == name)
                    .ok_or_else(|| {
                        bad(format!(
                            "`activity_merge` must be union or slot_first, not {name:?}"
                        ))
                    })?;
                out.push(SmartObjectDefOp::ActivityMerge(merge));
            }
            if let Some(name) = params.get("user_rule").and_then(Value::as_str) {
                let rule = RULES.into_iter().find(|r| rule_name(*r) == name).ok_or_else(|| {
                    bad(format!(
                        "`user_rule` must be definition_and_slot, slot_only or slot_else_definition, not {name:?}"
                    ))
                })?;
                out.push(SmartObjectDefOp::UserRule(rule));
            }
            if out.is_empty() {
                return Err(bad("`set_rules` needs activity_merge or user_rule"));
            }
        }
        "set_user_query" => {
            let query = match params.get("query") {
                None => return Err(bad("`query` (tag query RON text, or null) is required")),
                Some(Value::Null) => None,
                Some(Value::String(text)) => parse_query(text).map_err(|e| e.rpc(HINT))?,
                Some(_) => return Err(bad("`query` must be tag query RON text or null")),
            };
            out.push(SmartObjectDefOp::UserQuery {
                scope: scope_param(&def, params)?,
                query,
            });
        }
        "set_conditions" => out.push(SmartObjectDefOp::Conditions {
            scope: scope_param(&def, params)?,
            conditions: conditions_param(params)?,
        }),
        "set_behaviors" => out.push(SmartObjectDefOp::Behaviors {
            scope: scope_param(&def, params)?,
            behaviors: behaviors_param(params)?,
        }),
        "add_slot" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .map_or_else(|| unused_slot_name(&def), str::to_owned);
            out.push(SmartObjectDefOp::AddSlot { name });
            let index = def.slots.len();
            if let Some(offset) = offset_param(params, OffsetDef::default())? {
                out.push(SmartObjectDefOp::SlotOffset { slot: index, offset });
            }
        }
        "remove_slot" => out.push(SmartObjectDefOp::RemoveSlot(required_slot(&def, params)?)),
        "set_slot" => {
            let slot = required_slot(&def, params)?;
            let current = def
                .slots
                .get(slot)
                .ok_or_else(|| err("bad_index", format!("no slot {slot}")).rpc(HINT))?;
            if let Some(name) = params.get("name").and_then(Value::as_str) {
                out.push(SmartObjectDefOp::RenameSlot {
                    slot,
                    name: name.to_owned(),
                });
            }
            if let Some(enabled) = params.get("enabled").and_then(Value::as_bool) {
                out.push(SmartObjectDefOp::SlotEnabled { slot, enabled });
            }
            if let Some(tags) = strings(params, "initial_tags")? {
                out.push(SmartObjectDefOp::SlotInitialTags { slot, tags });
            }
            if let Some(offset) = offset_param(params, current.offset)? {
                out.push(SmartObjectDefOp::SlotOffset { slot, offset });
            }
            if out.is_empty() {
                return Err(bad(
                    "`set_slot` needs name, enabled, initial_tags, translation or rotation",
                ));
            }
        }
        "add_entrance" => {
            let slot = required_slot(&def, params)?;
            let base = EntranceDef {
                offset: OffsetDef::default(),
                kind: EntranceKindDef::Both,
                rank: 0,
            };
            out.push(SmartObjectDefOp::AddEntrance {
                slot,
                entrance: EntranceDef {
                    offset: offset_param(params, base.offset)?.unwrap_or(base.offset),
                    kind: kind_param(params)?.unwrap_or(base.kind),
                    rank: rank_param(params)?.unwrap_or(base.rank),
                },
            });
        }
        "remove_entrance" | "set_entrance" => {
            let slot = required_slot(&def, params)?;
            let index = params
                .get("entrance")
                .and_then(Value::as_u64)
                .ok_or_else(|| bad("`entrance` (an index) is required"))? as usize;
            if op == "remove_entrance" {
                out.push(SmartObjectDefOp::RemoveEntrance { slot, index });
            } else {
                let current = def
                    .slots
                    .get(slot)
                    .and_then(|s| s.entrances.get(index))
                    .cloned()
                    .ok_or_else(|| {
                        err("bad_index", format!("slot {slot} has no entrance {index}")).rpc(HINT)
                    })?;
                out.push(SmartObjectDefOp::Entrance {
                    slot,
                    index,
                    entrance: EntranceDef {
                        offset: offset_param(params, current.offset)?.unwrap_or(current.offset),
                        kind: kind_param(params)?.unwrap_or(current.kind),
                        rank: rank_param(params)?.unwrap_or(current.rank),
                    },
                });
            }
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "`op` must be get, open, set_activity, set_rules, set_user_query, set_conditions, \
                 set_behaviors, add_slot, remove_slot, set_slot, add_entrance, remove_entrance, \
                 set_entrance or save, got `{other}`"
            )));
        }
    }
    Ok(out)
}

fn with_undo_depth(world: &World, mut out: Value) -> Value {
    out["undo_depth"] = json!(world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth));
    out
}

/// `editor.smart_object_def_state`.
fn def_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(with_undo_depth(world, doc_json(world)))
}

/// `editor.smart_object_def_edit`.
fn def_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(path) = params.get("path").and_then(Value::as_str) {
        open_def(world, path).map_err(|e| e.rpc(HINT))?;
    }
    let op = params.get("op").and_then(Value::as_str).unwrap_or("get");
    let mut extra = json!({});
    match op {
        "get" | "open" => {
            if op == "open" && params.get("path").is_none() {
                return Err(bad("`open` needs `path`"));
            }
            current_def(world)?;
        }
        "save" => {
            let written = save_def(world).map_err(|e| e.rpc(HINT))?;
            extra = json!({ "written": written });
        }
        _ => {
            let ops = ops_of(world, op, params)?;
            // All or nothing: every op applies to a copy first.
            let mut probe = current_def(world)?;
            for o in &ops {
                apply_op(&mut probe, o).map_err(|e| e.rpc(HINT))?;
            }
            let mut changed = false;
            for o in &ops {
                changed |= edit_def(world, o).map_err(|e| e.rpc(HINT))?;
            }
            extra = json!({ "changed": changed });
        }
    }
    let mut out = doc_json(world);
    if let (Some(map), Some(more)) = (out.as_object_mut(), extra.as_object()) {
        map.extend(more.clone());
    }
    Ok(with_undo_depth(world, out))
}

const fn param(name: &'static str, kind: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required: false,
        description,
    }
}

const EDIT_PARAMS: &[ParamSchema] = &[
    param(
        "path",
        "string",
        "Project-relative .smartobject.ron to open first (a different path replaces the open definition).",
    ),
    param(
        "op",
        "string",
        "get (default), open, set_activity, set_rules, set_user_query, set_conditions, set_behaviors, add_slot, \
         remove_slot, set_slot, add_entrance, remove_entrance, set_entrance or save.",
    ),
    param(
        "slot",
        "any",
        "A slot index or name. set_activity, set_user_query, set_conditions, set_behaviors: omitted edits the \
         definition's own field.",
    ),
    param(
        "entrance",
        "number",
        "remove_entrance, set_entrance: the entrance index within the slot.",
    ),
    param("tags", "array", "set_activity: the activity tags, in order."),
    param("activity_merge", "string", "set_rules: union or slot_first."),
    param(
        "user_rule",
        "string",
        "set_rules: definition_and_slot, slot_only or slot_else_definition.",
    ),
    param(
        "query",
        "any",
        "set_user_query: tag query RON text (Has(\"Role.Guard\"), AnyOf([...])) or null for none.",
    ),
    param(
        "conditions",
        "array",
        "set_conditions: [{ path, params?: { key: value } }].",
    ),
    param(
        "behaviors",
        "array",
        "set_behaviors: [{ family, data? (RON text) }].",
    ),
    param(
        "name",
        "string",
        "add_slot (default slot_N), set_slot: the slot name.",
    ),
    param("enabled", "boolean", "set_slot: whether the slot starts enabled."),
    param(
        "initial_tags",
        "array",
        "set_slot: the runtime tags the slot starts with.",
    ),
    param(
        "translation",
        "array",
        "add_slot, set_slot, add_entrance, set_entrance: [x, y, z] (a slot in the object's space, an entrance in its slot's).",
    ),
    param(
        "rotation",
        "array",
        "add_slot, set_slot, add_entrance, set_entrance: [x, y, z, w] quaternion.",
    ),
    param(
        "kind",
        "string",
        "add_entrance, set_entrance: enter, exit or both.",
    ),
    param(
        "rank",
        "number",
        "add_entrance, set_entrance: preference, -128 to 127 (higher wins).",
    ),
];

const STATE_RESULT: &str = "{ open, path?, dirty?, valid?, validation?, comments_warning?, activity?, activity_merge?, \
    user_rule?, user_query?, conditions?: [{ path, params }], behaviors?: [{ family, data }], slots?: [{ name, \
    offset: { translation, rotation }, enabled, activity, initial_tags, user_query, conditions, behaviors, \
    entrances: [{ offset, kind, rank }] }], handles: { enabled, preview }, undo_depth }";

/// The definition editor's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.smart_object_def_state",
                tool: "smart_object_def_state",
                description: "The smart-object definition editor (spec 39 § 3): the open .smartobject.ron's activity, \
                    rules, queries, conditions, behaviors, slots and entrances, whether it is edited and valid \
                    (the runtime validator's message), the comment-loss warning and the slot handle state.",
                params: &[],
                result: STATE_RESULT,
                mutating: false,
                permission: Permission::Read,
            },
            def_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.smart_object_def_edit",
                tool: "smart_object_def_edit",
                description: "Opens a .smartobject.ron in the definition editor and edits it the way its forms do \
                    (each op one undo step, all or nothing per call), or saves it (an unedited definition keeps \
                    its bytes; an invalid one is refused with the runtime validator's message).",
                params: EDIT_PARAMS,
                result: "editor.smart_object_def_state's result plus changed? and written?",
                mutating: true,
                permission: Permission::Assets,
            },
            def_edit as Handler,
        ),
    ]
}

pub(crate) mod autosave;

#[cfg(test)]
mod tests;
