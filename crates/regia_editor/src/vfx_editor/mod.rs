// SPDX-License-Identifier: MPL-2.0
//! The VFX editor (`docs/specs/26-terrain-vfx-highend.md` § 3, ADR-3171): a `.vfx.ron` effect's
//! module stack per emitter, the curve and gradient editors, a live preview through
//! `regia_vfx`'s offscreen harness and per-emitter cost rows. Agent surface: `editor.vfx_open`,
//! `editor.vfx_state`, `editor.vfx_edit`, `editor.vfx_preview` (`book/agents/vfx.md`).
//!
//! The document is edited through `regia_vfx`'s own types (`VfxSystemDef` and the module structs),
//! checked after every change by the format's validator (`VfxSystemDef::validate`) and written with
//! `VfxSystemDef::to_ron`, the serializer the loader reads, atomically. Every change is one
//! [`VfxEdit`] on the editor's undo stack. An effect nobody edited is never rewritten, and an edit
//! undone back to the opened state saves the opened bytes again.
//!
//! Disabling a module (the A/B toggle) takes its values out of the effect into the document's stash
//! (`.vfx.ron` has no "enabled" field and stays frozen at v7): the preview, the simulation hash and a
//! save see the effect without it; enabling puts the exact values back.

pub mod agent;
pub(crate) mod autosave;
pub mod curves;
pub mod modules;
pub mod panel;
pub mod preview;
pub mod sim;

use regia_ecs::prelude::*;
use regia_vfx::format::{
    EmitterDef, InitModule, RangeDef, RenderModule, SpawnMode, SpawnModule, UpdateModule, VFX_FORMAT_VERSION,
    VfxSystemDef,
};

use crate::command::{EditorCommand, UndoStack};
use crate::data_panels::{PanelError, confined, write_atomic};

use curves::{CurveRef, GradientRef, KeyEdit, KeyError, apply_key_edit};
use modules::{ModuleData, ModuleKind, ParamError, ParamValue};
use serde::{Deserialize, Serialize};

/// The book page of the agent surface (error hints point here).
pub const HINT: &str = "see book/agents/vfx.md";

/// The suffix of an effect file.
pub const SUFFIX: &str = ".vfx.ron";

/// Most emitters one effect may hold in the editor.
pub const MAX_EMITTERS: usize = 64;

/// Every error code the editor returns (`data.reason` of an agent error), documented in
/// `book/agents/vfx.md`.
pub const ERROR_CODES: &[&str] = &[
    "vfx_editor_no_project",
    "vfx_editor_bad_path",
    "vfx_editor_io_failed",
    "vfx_editor_parse_failed",
    "vfx_editor_no_effect",
    "vfx_editor_unknown_emitter",
    "vfx_editor_unknown_module",
    "vfx_editor_unknown_param",
    "vfx_editor_unknown_curve",
    "vfx_editor_bad_value",
    "vfx_editor_bad_index",
    "vfx_editor_too_many",
    "vfx_editor_required_module",
    "vfx_editor_not_addable",
    "vfx_editor_already_present",
    "vfx_editor_not_present",
    "vfx_editor_module_disabled",
    "vfx_editor_invalid",
    "vfx_editor_serialize_failed",
    "vfx_editor_no_gpu",
    "vfx_editor_preview_failed",
    "vfx_editor_empty_bounds",
];

pub(crate) fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("vfx_editor_{code}"), message)
}

/// A module the user disabled: its values, out of the effect until it is enabled again. Part of
/// the editor-only autosave envelope (never written to `.vfx.ron`, which has no "enabled" field).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Disabled {
    /// The emitter it belongs to (by name; names are unique in an effect).
    pub emitter: String,
    /// The module's values.
    pub data: ModuleData,
}

/// What one undo step swaps: the effect and the stash of disabled modules.
#[derive(Clone, Debug, PartialEq)]
pub struct DocState {
    /// The effect as the preview, the simulation and a save see it.
    pub def: VfxSystemDef,
    /// Disabled modules.
    pub disabled: Vec<Disabled>,
}

/// The open effect.
#[derive(Clone, Debug)]
pub struct VfxDoc {
    /// Project-relative path of the `.vfx.ron`.
    pub path: String,
    /// The file's text when it was opened (byte-exact).
    pub opened_text: String,
    /// The definition that text parses to.
    pub opened_def: VfxSystemDef,
    /// The text of the last save (the opened text before any save).
    pub saved_text: String,
    /// The definition of the last save.
    pub saved_def: VfxSystemDef,
    /// The state being edited.
    pub state: DocState,
    /// Bumped on every change (and undo/redo): the preview restarts when it moves.
    pub revision: u64,
}

impl VfxDoc {
    /// Whether the effect differs from the file as last saved.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.state.def != self.saved_def
    }

    /// The modules of emitter `index` in stack order: `(kind, enabled)`, present and disabled ones.
    #[must_use]
    pub fn stack(&self, index: usize) -> Vec<(ModuleKind, bool)> {
        let Some(e) = self.state.def.emitters.get(index) else {
            return Vec::new();
        };
        modules::ALL_KINDS
            .into_iter()
            .filter_map(|k| {
                if modules::present(e, k) {
                    Some((k, true))
                } else if self.is_disabled(&e.name, k) {
                    Some((k, false))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Whether module `kind` of emitter `emitter` is disabled.
    #[must_use]
    pub fn is_disabled(&self, emitter: &str, kind: ModuleKind) -> bool {
        self.state
            .disabled
            .iter()
            .any(|d| d.emitter == emitter && d.data.kind() == kind)
    }

    /// The text a save writes now: the opened bytes when the effect is back at its opened state,
    /// the last saved bytes when it is back at that, else the canonical serialization.
    ///
    /// # Errors
    ///
    /// `vfx_editor_serialize_failed`.
    pub fn text_to_save(&self) -> Result<String, PanelError> {
        if self.state.def == self.opened_def {
            return Ok(self.opened_text.clone());
        }
        if self.state.def == self.saved_def {
            return Ok(self.saved_text.clone());
        }
        self.state
            .def
            .to_ron()
            .map_err(|e| err("serialize_failed", e.to_string()))
    }
}

/// The VFX editor's state (a resource of the edit world).
#[derive(Resource, Debug, Default)]
pub struct VfxEditorPanel {
    doc: Option<VfxDoc>,
    next_revision: u64,
    /// The preview session (GPU), rebuilt when the effect changes.
    pub(crate) preview: Option<preview::PreviewSession>,
    /// Preview controls.
    pub(crate) ui: panel::PanelUi,
    last_error: Option<String>,
}

impl VfxEditorPanel {
    /// The open effect.
    #[must_use]
    pub fn doc(&self) -> Option<&VfxDoc> {
        self.doc.as_ref()
    }

    fn bump(&mut self) -> u64 {
        self.next_revision += 1;
        self.next_revision
    }
}

/// One change to the open effect: what a widget or `editor.vfx_edit` asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum VfxChange {
    /// Adds module `kind` (palette defaults) to emitter `emitter`.
    AddModule(usize, ModuleKind),
    /// Removes module `kind` (enabled or disabled) from emitter `emitter`.
    RemoveModule(usize, ModuleKind),
    /// Enables or disables module `kind` of emitter `emitter` (the A/B toggle).
    SetEnabled(usize, ModuleKind, bool),
    /// Sets one parameter of module `kind` of emitter `emitter`.
    SetParam(usize, ModuleKind, String, ParamValue),
    /// A curve or gradient key edit on emitter `emitter`.
    Key(usize, KeyEdit),
    /// Appends a new emitter with this name (a 20/s point fountain).
    AddEmitter(String),
    /// Removes emitter `index` (its disabled modules go with it).
    RemoveEmitter(usize),
    /// Moves emitter `from` to position `to`.
    MoveEmitter(usize, usize),
    /// The system's particle budget per instance.
    Budget(u32),
    /// The system's authored fixed bounds (`.vfx.ron` v8 `system.bounds`, P-225); `None` clears
    /// them (the computed bounds cull again).
    FixedBounds(Option<regia_vfx::format::FixedBoundsDef>),
}

impl VfxChange {
    /// Coalescing key: consecutive changes with one key (a slider or key drag) are one undo step.
    fn key(&self) -> Option<String> {
        match self {
            Self::SetParam(e, k, p, _) => Some(format!("param:{e}:{}:{p}", k.name())),
            Self::Key(e, KeyEdit::MoveKey(c, ..)) => Some(format!("key:{e}:{}", c.name())),
            Self::Key(e, KeyEdit::MoveColor(g, ..)) => Some(format!("color:{e}:{}", g.name())),
            Self::Budget(_) => Some("budget".into()),
            // A drag of one box field is one undo step; setting or clearing the box is its own.
            Self::FixedBounds(Some(_)) => Some("fixed_bounds".into()),
            _ => None,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::AddModule(..) => "add module",
            Self::RemoveModule(..) => "remove module",
            Self::SetEnabled(_, _, true) => "enable module",
            Self::SetEnabled(_, _, false) => "disable module",
            Self::SetParam(..) => "parameter",
            Self::Key(..) => "curve key",
            Self::AddEmitter(_) => "add emitter",
            Self::RemoveEmitter(_) => "remove emitter",
            Self::MoveEmitter(..) => "move emitter",
            Self::Budget(_) => "budget",
            Self::FixedBounds(Some(_)) => "fixed bounds",
            Self::FixedBounds(None) => "clear fixed bounds",
        }
    }
}

/// The emitter a new [`VfxChange::AddEmitter`] starts as: 20 particles a second from a point, up at
/// 2 m/s, one second of life, soft additive billboards.
#[must_use]
pub fn new_emitter(name: &str) -> EmitterDef {
    EmitterDef {
        name: name.to_owned(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Rate(20.0),
            lifetime: RangeDef { min: 1.0, max: 1.0 },
            count_scale: 1.0,
        },
        init: InitModule {
            shape: regia_vfx::format::ShapeDef::Point,
            velocity: regia_vfx::format::VelocityDef {
                mode: regia_vfx::format::VelocityMode::Directional {
                    dir: (0.0, 0.0, 1.0),
                    spread: 20.0,
                    speed: RangeDef { min: 2.0, max: 2.0 },
                },
                inherit: 0.0,
            },
            size: RangeDef { min: 0.1, max: 0.2 },
            rotation: RangeDef::default(),
            spin: RangeDef::default(),
            color: Vec::new(),
        },
        update: UpdateModule::default(),
        render: RenderModule::default(),
    }
}

fn emitter_name(state: &DocState, index: usize) -> Result<String, PanelError> {
    state
        .def
        .emitters
        .get(index)
        .map(|e| e.name.clone())
        .ok_or_else(|| {
            err(
                "unknown_emitter",
                format!("no emitter {index} (the effect has {})", state.def.emitters.len()),
            )
        })
}

fn curve_module(c: CurveRef) -> Option<ModuleKind> {
    match c {
        CurveRef::SizeOverLife => Some(ModuleKind::SizeOverLife),
        CurveRef::AlphaOverLife => Some(ModuleKind::AlphaOverLife),
        CurveRef::RotationOverLife => Some(ModuleKind::RotationOverLife),
        CurveRef::RibbonWidth => None,
    }
}

fn gradient_module(g: GradientRef) -> Option<ModuleKind> {
    match g {
        GradientRef::InitColor => Some(ModuleKind::InitColor),
        GradientRef::ColorOverLife => Some(ModuleKind::ColorOverLife),
        GradientRef::RibbonColor => None,
    }
}

fn key_module(edit: &KeyEdit) -> Option<ModuleKind> {
    match edit {
        KeyEdit::AddKey(c, ..) | KeyEdit::MoveKey(c, ..) | KeyEdit::DeleteKey(c, ..) => curve_module(*c),
        KeyEdit::AddColor(g, ..) | KeyEdit::MoveColor(g, ..) | KeyEdit::DeleteColor(g, ..) => {
            gradient_module(*g)
        }
    }
}

/// Applies `change` to `state` and validates the result with the format's validator.
///
/// # Errors
///
/// The codes in [`ERROR_CODES`]: `vfx_editor_invalid` carries the validator's message.
pub fn apply_change(state: &mut DocState, change: &VfxChange) -> Result<(), PanelError> {
    match change {
        VfxChange::AddModule(i, kind) => {
            let name = emitter_name(state, *i)?;
            if kind.required() {
                return Err(err(
                    "required_module",
                    format!("`{}` is part of every emitter", kind.name()),
                ));
            }
            if !kind.addable() {
                return Err(err(
                    "not_addable",
                    format!(
                        "`{}` needs a child emitter: write the link in the .vfx.ron",
                        kind.name()
                    ),
                ));
            }
            let e = &mut state.def.emitters[*i];
            if modules::present(e, *kind)
                || state
                    .disabled
                    .iter()
                    .any(|d| d.emitter == name && d.data.kind() == *kind)
            {
                return Err(err(
                    "already_present",
                    format!("emitter `{name}` already has `{}`", kind.name()),
                ));
            }
            if let Some(data) = ModuleData::default_for(*kind) {
                modules::put(e, data);
            }
        }
        VfxChange::RemoveModule(i, kind) => {
            let name = emitter_name(state, *i)?;
            if kind.required() {
                return Err(err(
                    "required_module",
                    format!("`{}` is part of every emitter", kind.name()),
                ));
            }
            let before = state.disabled.len();
            state
                .disabled
                .retain(|d| !(d.emitter == name && d.data.kind() == *kind));
            let removed_stash = state.disabled.len() != before;
            let taken = modules::take(&mut state.def.emitters[*i], *kind).is_some();
            if !taken && !removed_stash {
                return Err(err(
                    "not_present",
                    format!("emitter `{name}` has no `{}`", kind.name()),
                ));
            }
        }
        VfxChange::SetEnabled(i, kind, enabled) => {
            let name = emitter_name(state, *i)?;
            if kind.required() {
                return Err(err(
                    "required_module",
                    format!(
                        "`{}` is part of every emitter and cannot be disabled",
                        kind.name()
                    ),
                ));
            }
            let e = &mut state.def.emitters[*i];
            if *enabled {
                let Some(pos) = state
                    .disabled
                    .iter()
                    .position(|d| d.emitter == name && d.data.kind() == *kind)
                else {
                    if modules::present(e, *kind) {
                        return Ok(());
                    }
                    return Err(err(
                        "not_present",
                        format!("emitter `{name}` has no `{}`", kind.name()),
                    ));
                };
                let d = state.disabled.remove(pos);
                modules::put(e, d.data);
            } else if let Some(data) = modules::take(e, *kind) {
                state.disabled.push(Disabled { emitter: name, data });
            } else if !state
                .disabled
                .iter()
                .any(|d| d.emitter == name && d.data.kind() == *kind)
            {
                return Err(err(
                    "not_present",
                    format!("emitter `{name}` has no `{}`", kind.name()),
                ));
            }
        }
        VfxChange::SetParam(i, kind, param, value) => {
            let name = emitter_name(state, *i)?;
            if state
                .disabled
                .iter()
                .any(|d| d.emitter == name && d.data.kind() == *kind)
            {
                return Err(err(
                    "module_disabled",
                    format!("`{}` of `{name}` is disabled: enable it to edit it", kind.name()),
                ));
            }
            let e = &mut state.def.emitters[*i];
            if !modules::present(e, *kind) {
                return Err(err(
                    "not_present",
                    format!("emitter `{name}` has no `{}`", kind.name()),
                ));
            }
            modules::set_param(e, *kind, param, value).map_err(|pe| match pe {
                ParamError::Unknown(m) => err("unknown_param", m),
                ParamError::BadValue(m) => err("bad_value", m),
            })?;
        }
        VfxChange::Key(i, edit) => {
            let name = emitter_name(state, *i)?;
            if let Some(kind) = key_module(edit)
                && state
                    .disabled
                    .iter()
                    .any(|d| d.emitter == name && d.data.kind() == kind)
            {
                return Err(err(
                    "module_disabled",
                    format!(
                        "`{}` of `{name}` is disabled: enable it to edit its keys",
                        kind.name()
                    ),
                ));
            }
            apply_key_edit(&mut state.def.emitters[*i], edit).map_err(|ke| match ke {
                KeyError::BadValue(m) => err("bad_value", m),
                KeyError::BadIndex(m) => err("bad_index", m),
                KeyError::TooMany(m) => err("too_many", m),
            })?;
        }
        VfxChange::AddEmitter(name) => {
            let name = name.trim();
            if name.is_empty() {
                return Err(err("bad_value", "an emitter needs a name"));
            }
            if state.def.emitters.len() >= MAX_EMITTERS {
                return Err(err(
                    "too_many",
                    format!("an effect holds at most {MAX_EMITTERS} emitters in the editor"),
                ));
            }
            state.def.emitters.push(new_emitter(name));
        }
        VfxChange::RemoveEmitter(i) => {
            let name = emitter_name(state, *i)?;
            state.def.emitters.remove(*i);
            state.disabled.retain(|d| d.emitter != name);
        }
        VfxChange::MoveEmitter(from, to) => {
            emitter_name(state, *from)?;
            let len = state.def.emitters.len();
            if *to >= len {
                return Err(err(
                    "bad_index",
                    format!("no position {to} (the effect has {len} emitters)"),
                ));
            }
            let e = state.def.emitters.remove(*from);
            state.def.emitters.insert(*to, e);
        }
        VfxChange::Budget(b) => {
            if *b == 0 {
                return Err(err("bad_value", "`budget` must be at least 1"));
            }
            state.def.system.budget = *b;
        }
        VfxChange::FixedBounds(bounds) => {
            if bounds.is_some_and(|b| !b.is_valid()) {
                return Err(err(
                    "bad_value",
                    "fixed bounds must be finite with `min` <= `max` on every axis",
                ));
            }
            state.def.system.bounds = *bounds;
        }
    }
    state.def.validate().map_err(|e| err("invalid", e.to_string()))
}

/// One reversible change (one undo step) to the effect at `path`. Undo and redo leave the open
/// document alone when another effect has been opened since.
pub struct VfxEdit {
    path: String,
    before: DocState,
    after: DocState,
    key: Option<String>,
    label: &'static str,
}

impl VfxEdit {
    fn set(world: &mut World, path: &str, state: &DocState) {
        let Some(mut panel) = world.get_resource_mut::<VfxEditorPanel>() else {
            return;
        };
        if panel.doc.as_ref().is_none_or(|doc| doc.path != path) {
            // The step belongs to an effect that is no longer open: nothing to change.
            return;
        }
        let revision = panel.bump();
        if let Some(doc) = panel.doc.as_mut() {
            doc.state = state.clone();
            doc.revision = revision;
        }
    }
}

impl EditorCommand for VfxEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, &self.path, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, &self.path, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<VfxEdit>() else {
            return false;
        };
        if self.key.is_some() && self.key == next.key && self.path == next.path {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        format!("VFX: {}", self.label)
    }
}

/// Opens the effect at the project-relative `path`.
///
/// # Errors
///
/// `vfx_editor_no_project`, `vfx_editor_bad_path`, `vfx_editor_io_failed` or
/// `vfx_editor_parse_failed` (a file the loader refuses, with the loader's message).
pub fn open_effect(world: &mut World, path: &str) -> Result<(), PanelError> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| err("no_project", "no project is open; open one first"))?;
    let full = confined(&root, path, &[SUFFIX], "vfx_editor")?;
    let text =
        std::fs::read_to_string(&full).map_err(|e| err("io_failed", format!("{}: {e}", full.display())))?;
    let def = VfxSystemDef::from_ron(&text).map_err(|e| err("parse_failed", format!("{path}: {e}")))?;
    let mut panel = world.remove_resource::<VfxEditorPanel>().unwrap_or_default();
    let revision = panel.bump();
    path.clone_into(&mut panel.ui.path_input);
    panel.doc = Some(VfxDoc {
        path: path.to_owned(),
        opened_text: text.clone(),
        opened_def: def.clone(),
        saved_text: text,
        saved_def: def.clone(),
        state: DocState {
            def,
            disabled: Vec::new(),
        },
        revision,
    });
    panel.preview = None;
    panel.ui.selected = 0;
    panel.ui.preview_disabled.clear();
    panel.ui.preview_solo.clear();
    panel.last_error = None;
    world.insert_resource(panel);
    Ok(())
}

/// The open document's state.
fn current(world: &World) -> Result<DocState, PanelError> {
    world
        .get_resource::<VfxEditorPanel>()
        .and_then(|p| p.doc.as_ref())
        .map(|d| d.state.clone())
        .ok_or_else(|| err("no_effect", "no effect is open; pass `path` to editor.vfx_open"))
}

/// Applies `change` to the open effect as one undo step. `Ok(false)` when it changed nothing. A
/// refused change leaves the document, the undo stack and the file untouched.
///
/// # Errors
///
/// `vfx_editor_no_effect`, else [`apply_change`]'s errors.
pub fn edit_effect(world: &mut World, change: &VfxChange) -> Result<bool, PanelError> {
    let before = current(world)?;
    let path = world
        .get_resource::<VfxEditorPanel>()
        .and_then(|p| p.doc.as_ref())
        .map(|d| d.path.clone())
        .unwrap_or_default();
    let mut after = before.clone();
    apply_change(&mut after, change)?;
    if after == before {
        return Ok(false);
    }
    let mut command = VfxEdit {
        path,
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

/// What a save did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveReport {
    /// Bytes written (0 when the file already held them).
    pub written: usize,
    /// Disabled modules the file does not hold (they stay in the editor's stash).
    pub disabled_not_saved: usize,
}

/// Saves the open effect atomically. A file that already holds the text to save is not touched.
///
/// # Errors
///
/// `vfx_editor_no_effect`, `vfx_editor_no_project`, `vfx_editor_serialize_failed`,
/// `vfx_editor_invalid` (never expected: every edit is validated) or `vfx_editor_io_failed`.
pub fn save_effect(world: &mut World) -> Result<SaveReport, PanelError> {
    let _span = tracing::info_span!("vfx_editor.save").entered();
    let root =
        crate::mesh_source::project_root(world).ok_or_else(|| err("no_project", "no project is open"))?;
    let mut panel = world.remove_resource::<VfxEditorPanel>().unwrap_or_default();
    let result = (|| -> Result<_, PanelError> {
        let doc = panel
            .doc
            .as_mut()
            .ok_or_else(|| err("no_effect", "no effect is open"))?;
        doc.state
            .def
            .validate()
            .map_err(|e| err("invalid", e.to_string()))?;
        let text = doc.text_to_save()?;
        let full = confined(&root, &doc.path, &[SUFFIX], "vfx_editor")?;
        let on_disk = std::fs::read_to_string(&full).ok();
        let written = if on_disk.as_deref() == Some(text.as_str()) {
            0
        } else {
            write_atomic(&full, &text, "vfx_editor")?;
            text.len()
        };
        doc.saved_text = text;
        doc.saved_def = doc.state.def.clone();
        Ok(SaveReport {
            written,
            disabled_not_saved: doc.state.disabled.len(),
        })
    })();
    world.insert_resource(panel);
    result
}

/// Undoes the last editor step (any panel's: the stack is shared). `true` when one was undone.
pub fn undo(world: &mut World) -> bool {
    if !world.contains_resource::<UndoStack>() {
        return false;
    }
    world.resource_scope::<UndoStack, bool>(|w, mut s| s.undo(w))
}

/// Redoes the last undone editor step. `true` when one was redone.
pub fn redo(world: &mut World) -> bool {
    if !world.contains_resource::<UndoStack>() {
        return false;
    }
    world.resource_scope::<UndoStack, bool>(|w, mut s| s.redo(w))
}

/// A fresh effect file's text: one emitter, the current format version.
#[must_use]
pub fn template(name: &str) -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: name.to_owned(),
        system: regia_vfx::format::SystemModule::default(),
        emitters: vec![new_emitter("main")],
    }
}

#[cfg(test)]
mod tests;
