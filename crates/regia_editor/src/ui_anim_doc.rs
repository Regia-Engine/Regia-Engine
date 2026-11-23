// SPDX-License-Identifier: MPL-2.0
//! The UI animation document (`.uianim.ron`) the UI Animation panel and `editor.ui_anim_*` edit
//! (ADR-3092, `book/agents/ui-anim-designer.md`).
//!
//! [`UiAnimDoc`] holds a [`UiAnimLibrary`], one undo step per edit ([`AnimOp`]), and writes the file
//! atomically (a temp file and a rename). Every edit is checked with the format's own validation on a
//! scratch copy first, so the document is always one the runtime would load; an edit that would break
//! an animation is refused with the existing [`regia_ui::anim_timeline::AnimError`] text. Scrubbing
//! compiles the animation with [`UiAnimDef::compile`] and samples the [`TweenPlan`](regia_ui::tween::TweenPlan)
//! and never touches the document or the designer's `.ui.ron`.
//!
//! An animation named `"<node>::<label>"` targets the node named `<node>`
//! (`regia_asset_pipeline::ui_anim_validate`); the format has no node field.

use std::path::{Path, PathBuf};

use regia_asset_pipeline::ui_anim_validate::{UiNodeFact, target_of};
use regia_ecs::prelude::{Resource, World, bevy_ecs};
use regia_ui::anim_timeline::{
    AnimEvent, AnimKey, AnimTrack, AnimValue, LoopMode, MAX_LOOPS, SPEED_RANGE, UiAnimDef, UiAnimLibrary,
};
use regia_ui::authoring::{BackgroundDef, UiFile, UiNodeDef};
use regia_ui::style::Dimension;
use regia_ui::tween::{Ease, TweenProperty, TweenValue};
use serde_json::{Value, json};

use crate::shared_panel_history::{SharedDoc, SharedHistory, StepKind};
use crate::ui_designer::walk;

/// Undo snapshots kept per document.
pub const UNDO_DEPTH: usize = 200;

/// A failure: a stable code (`ui_anim_<code>` on the agent surface) and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiAnimError {
    /// `no_document`, `unknown_animation`, `unknown_track`, `unknown_key`, `unknown_event`,
    /// `duplicate_animation`, `bad_name`, `bad_value`, `invalid`, `parse`, `io`, `nothing_to_undo`,
    /// `nothing_to_redo`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl UiAnimError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for UiAnimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// One edit; each is one undo step.
#[derive(Debug, Clone, PartialEq)]
pub enum AnimOp {
    /// A new animation with one track (`property`, default `opacity`) holding one key at 0 s.
    AddAnimation {
        /// The new name.
        name: String,
        /// The first track's property.
        property: Option<String>,
    },
    /// Renames an animation.
    RenameAnimation {
        /// The current name.
        from: String,
        /// The new name.
        to: String,
    },
    /// Deletes an animation.
    RemoveAnimation(String),
    /// Sets `mode`, `loops` and `speed` (each only when given).
    SetPlayback {
        /// The animation.
        name: String,
        /// New mode.
        mode: Option<LoopMode>,
        /// New pass count.
        loops: Option<u32>,
        /// New speed.
        speed: Option<f32>,
    },
    /// Adds a track with one key at 0 s.
    AddTrack {
        /// The animation.
        name: String,
        /// The property.
        property: String,
    },
    /// Removes a track.
    RemoveTrack {
        /// The animation.
        name: String,
        /// The property.
        property: String,
    },
    /// Sets the key at `key.time` (replacing the one there, else inserting in time order). The track
    /// is created when the animation has none for the property.
    SetKey {
        /// The animation.
        name: String,
        /// The property.
        property: String,
        /// The key.
        key: AnimKey,
    },
    /// Removes the key at `time`.
    RemoveKey {
        /// The animation.
        name: String,
        /// The property.
        property: String,
        /// Its time.
        time: f32,
    },
    /// Replaces the frame event at `index`, or appends one when `index` is `None` (the event
    /// track, `.uianim.ron` v2, ADR-8900).
    SetEvent {
        /// The animation.
        name: String,
        /// The event's place in the animation's `events` list; `None` adds one at the end.
        index: Option<usize>,
        /// The event.
        event: AnimEvent,
    },
    /// Removes the frame event at `index`.
    RemoveEvent {
        /// The animation.
        name: String,
        /// The event's place in the `events` list.
        index: usize,
    },
}

/// The open library.
#[derive(Debug, Clone)]
pub struct UiAnimDoc {
    /// Where it was opened from / saves to.
    pub path: Option<PathBuf>,
    lib: UiAnimLibrary,
    saved: UiAnimLibrary,
    revision: u64,
    undo: Vec<UiAnimLibrary>,
    redo: Vec<UiAnimLibrary>,
    /// The animation the panel shows.
    pub selected: Option<String>,
    /// This document's identity on the shared history (P-322): a new one per open.
    serial: u64,
    /// The edits on their way to the editor's shared history.
    shared_history: SharedHistory<UiAnimLibrary>,
}

fn default_value(property: TweenProperty) -> AnimValue {
    if property.is_color() {
        AnimValue::Color([1.0; 4])
    } else if matches!(property, TweenProperty::Opacity | TweenProperty::Scale) {
        AnimValue::Number(1.0)
    } else {
        AnimValue::Number(0.0)
    }
}

fn parse_property(name: &str) -> Result<TweenProperty, UiAnimError> {
    TweenProperty::parse(name).ok_or_else(|| {
        let known: Vec<&str> = TweenProperty::ALL.iter().map(|p| p.name()).collect();
        UiAnimError::new(
            "invalid",
            format!("unknown property `{name}` (one of {})", known.join(", ")),
        )
    })
}

fn animation<'a>(lib: &'a mut UiAnimLibrary, name: &str) -> Result<&'a mut UiAnimDef, UiAnimError> {
    lib.animations
        .get_mut(name)
        .ok_or_else(|| UiAnimError::new("unknown_animation", format!("no animation named `{name}`")))
}

fn track<'a>(def: &'a mut UiAnimDef, property: &str) -> Result<&'a mut AnimTrack, UiAnimError> {
    def.tracks
        .iter_mut()
        .find(|t| t.property == property)
        .ok_or_else(|| UiAnimError::new("unknown_track", format!("no track for `{property}`")))
}

fn check_name(name: &str) -> Result<(), UiAnimError> {
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(UiAnimError::new(
            "bad_name",
            "an animation name is 1 to 128 characters without control characters",
        ));
    }
    Ok(())
}

/// Applies one op to `lib` without validating the result; the animation it touched, `None` for a
/// removal. `UiAnimDoc::apply_all` validates the touched animations once after its last op.
fn edit_op(lib: &mut UiAnimLibrary, op: AnimOp) -> Result<Option<String>, UiAnimError> {
    let touched = match op {
        AnimOp::AddAnimation { name, property } => {
            check_name(&name)?;
            if lib.animations.contains_key(&name) {
                return Err(UiAnimError::new(
                    "duplicate_animation",
                    format!("`{name}` already exists"),
                ));
            }
            let property = property.unwrap_or_else(|| "opacity".to_owned());
            let value = default_value(parse_property(&property)?);
            lib.animations.insert(
                name.clone(),
                UiAnimDef {
                    tracks: vec![AnimTrack {
                        property,
                        keys: vec![AnimKey {
                            time: 0.0,
                            value,
                            ease: None,
                        }],
                    }],
                    mode: LoopMode::Once,
                    loops: 1,
                    speed: 1.0,
                    events: Vec::new(),
                },
            );
            name
        }
        AnimOp::RenameAnimation { from, to } => {
            check_name(&to)?;
            if from != to && lib.animations.contains_key(&to) {
                return Err(UiAnimError::new(
                    "duplicate_animation",
                    format!("`{to}` already exists"),
                ));
            }
            let def = lib.animations.remove(&from).ok_or_else(|| {
                UiAnimError::new("unknown_animation", format!("no animation named `{from}`"))
            })?;
            lib.animations.insert(to.clone(), def);
            to
        }
        AnimOp::RemoveAnimation(name) => {
            lib.animations.remove(&name).ok_or_else(|| {
                UiAnimError::new("unknown_animation", format!("no animation named `{name}`"))
            })?;
            return Ok(None);
        }
        AnimOp::SetPlayback {
            name,
            mode,
            loops,
            speed,
        } => {
            let def = animation(lib, &name)?;
            if let Some(mode) = mode {
                def.mode = mode;
            }
            if let Some(loops) = loops {
                def.loops = loops;
            }
            if let Some(speed) = speed {
                def.speed = speed;
            }
            name
        }
        AnimOp::AddTrack { name, property } => {
            let value = default_value(parse_property(&property)?);
            let def = animation(lib, &name)?;
            if def.tracks.iter().any(|t| t.property == property) {
                return Err(UiAnimError::new(
                    "invalid",
                    format!("`{name}` already has a `{property}` track"),
                ));
            }
            def.tracks.push(AnimTrack {
                property,
                keys: vec![AnimKey {
                    time: 0.0,
                    value,
                    ease: None,
                }],
            });
            name
        }
        AnimOp::RemoveTrack { name, property } => {
            let def = animation(lib, &name)?;
            track(def, &property)?;
            def.tracks.retain(|t| t.property != property);
            name
        }
        AnimOp::SetKey { name, property, key } => {
            parse_property(&property)?;
            let def = animation(lib, &name)?;
            if !def.tracks.iter().any(|t| t.property == property) {
                def.tracks.push(AnimTrack {
                    property: property.clone(),
                    keys: Vec::new(),
                });
            }
            let t = track(def, &property)?;
            match t.keys.iter().position(|k| k.time == key.time) {
                Some(at) => t.keys[at] = key,
                None => {
                    let at = t.keys.partition_point(|k| k.time < key.time);
                    t.keys.insert(at, key);
                }
            }
            name
        }
        AnimOp::SetEvent { name, index, event } => {
            let def = animation(lib, &name)?;
            match index {
                Some(i) => {
                    let slot = def.events.get_mut(i).ok_or_else(|| {
                        UiAnimError::new("unknown_event", format!("`{name}` has no event {i}"))
                    })?;
                    *slot = event;
                }
                None => def.events.push(event),
            }
            name
        }
        AnimOp::RemoveEvent { name, index } => {
            let def = animation(lib, &name)?;
            if index >= def.events.len() {
                return Err(UiAnimError::new(
                    "unknown_event",
                    format!("`{name}` has no event {index}"),
                ));
            }
            def.events.remove(index);
            name
        }
        AnimOp::RemoveKey { name, property, time } => {
            let def = animation(lib, &name)?;
            let t = track(def, &property)?;
            let at = t.keys.iter().position(|k| k.time == time).ok_or_else(|| {
                UiAnimError::new("unknown_key", format!("`{property}` has no key at {time} s"))
            })?;
            t.keys.remove(at);
            name
        }
    };
    Ok(Some(touched))
}

/// Validates the animation `name` when the library still holds it (a later op of the same step may
/// have renamed or removed it).
fn check_animation(lib: &UiAnimLibrary, name: &str) -> Result<(), UiAnimError> {
    match lib.animations.get(name) {
        Some(def) => def
            .validate()
            .map_err(|e| UiAnimError::new("invalid", e.to_string())),
        None => Ok(()),
    }
}

impl UiAnimDoc {
    /// An empty library that will save to `path` when given.
    #[must_use]
    pub fn new_document(path: Option<PathBuf>) -> Self {
        let lib = UiAnimLibrary::default();
        Self {
            path,
            saved: lib.clone(),
            lib,
            revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            selected: None,
            serial: next_serial(),
            shared_history: SharedHistory::default(),
        }
    }

    /// Parses `text` (the format's own checks: a malformed or newer file is refused with its
    /// `AnimError` text and `parse` code).
    pub fn open_text(text: &str, path: Option<PathBuf>) -> Result<Self, UiAnimError> {
        let lib = UiAnimLibrary::from_ron(text).map_err(|e| UiAnimError::new("parse", e.to_string()))?;
        let selected = lib.animations.keys().next().cloned();
        Ok(Self {
            path,
            saved: lib.clone(),
            lib,
            revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            selected,
            serial: next_serial(),
            shared_history: SharedHistory::default(),
        })
    }

    /// Reads and parses `path`.
    pub fn open(path: &Path) -> Result<Self, UiAnimError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| UiAnimError::new("io", format!("{}: {e}", path.display())))?;
        Self::open_text(&text, Some(path.to_path_buf()))
    }

    /// The library.
    #[must_use]
    pub fn library(&self) -> &UiAnimLibrary {
        &self.lib
    }

    /// Counts up with every edit, undo and redo.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The library differs from what was last read or saved.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.lib != self.saved
    }

    /// Undo steps available.
    #[must_use]
    pub fn undo_len(&self) -> usize {
        if self.shared_history.is_shared() {
            return self.shared_history.undo_depth();
        }
        self.undo.len()
    }

    /// Redo steps available.
    #[must_use]
    pub fn redo_len(&self) -> usize {
        if self.shared_history.is_shared() {
            return self.shared_history.redo_depth();
        }
        self.redo.len()
    }

    /// Applies `op` as one undo step. Nothing changes when it fails.
    pub fn apply(&mut self, op: AnimOp) -> Result<(), UiAnimError> {
        self.apply_all(vec![op])
    }

    /// Applies `ops` in order as one undo step, all or nothing: when one fails, or an animation they
    /// touched is invalid after the last one, nothing changes and the error is returned. A key drag
    /// (remove the key at its old time, set it at the new one) is one step this way, a refused set
    /// cannot leave the key removed, and a track's only key can move.
    pub fn apply_all(&mut self, ops: Vec<AnimOp>) -> Result<(), UiAnimError> {
        let _span = tracing::debug_span!(target: "regia_editor", "ui_anim_edit").entered();
        let mut scratch = self.lib.clone();
        let mut selected = self.selected.clone();
        let mut touched = None;
        let mut check = Vec::new();
        for op in ops {
            let renamed = match &op {
                AnimOp::RenameAnimation { from, to } if selected.as_deref() == Some(from) => Some(to.clone()),
                _ => None,
            };
            let removed = match &op {
                AnimOp::RemoveAnimation(name) => Some(name.clone()),
                _ => None,
            };
            if let Some(name) = edit_op(&mut scratch, op)? {
                check.push(name.clone());
                touched = Some(name);
            }
            if let Some(to) = renamed {
                selected = Some(to);
            }
            if removed.is_some() && selected == removed {
                selected = scratch.animations.keys().next().cloned();
            }
        }
        // Validated once, after the last op: the remove half of a key move may empty a track that
        // the set half fills again.
        for name in &check {
            check_animation(&scratch, name)?;
        }
        if scratch == self.lib {
            return Ok(());
        }
        let path = self.history_path();
        self.shared_history.record(
            self.serial,
            &path,
            &self.lib,
            &scratch,
            StepKind::Discrete,
            "library",
        );
        let before = std::mem::replace(&mut self.lib, scratch);
        if !self.shared_history.is_shared() {
            self.undo.push(before);
            if self.undo.len() > UNDO_DEPTH {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.revision += 1;
        self.selected = selected;
        if let Some(name) = touched
            && self.selected.is_none()
        {
            self.selected = Some(name);
        }
        Ok(())
    }

    /// Replaces the library with the recovered one (a document autosave restore): the document is
    /// dirty until a save (what was opened stays the saved copy), and the undo and redo stacks
    /// are cleared.
    pub fn restore_library(&mut self, lib: UiAnimLibrary) {
        self.lib = lib;
        self.undo.clear();
        self.redo.clear();
        self.revision += 1;
        self.fix_selection();
    }

    /// Steps back one edit.
    pub fn undo(&mut self) -> Result<(), UiAnimError> {
        if self.shared_history.is_shared() {
            return if self.shared_history.navigate(false) {
                Ok(())
            } else {
                Err(UiAnimError::new("nothing_to_undo", "nothing to undo"))
            };
        }
        let previous = self
            .undo
            .pop()
            .ok_or_else(|| UiAnimError::new("nothing_to_undo", "nothing to undo"))?;
        self.redo.push(std::mem::replace(&mut self.lib, previous));
        self.revision += 1;
        self.fix_selection();
        Ok(())
    }

    /// Steps forward one undone edit.
    pub fn redo(&mut self) -> Result<(), UiAnimError> {
        if self.shared_history.is_shared() {
            return if self.shared_history.navigate(true) {
                Ok(())
            } else {
                Err(UiAnimError::new("nothing_to_redo", "nothing to redo"))
            };
        }
        let next = self
            .redo
            .pop()
            .ok_or_else(|| UiAnimError::new("nothing_to_redo", "nothing to redo"))?;
        self.undo.push(std::mem::replace(&mut self.lib, next));
        self.revision += 1;
        self.fix_selection();
        Ok(())
    }

    fn fix_selection(&mut self) {
        if self
            .selected
            .as_ref()
            .is_none_or(|s| !self.lib.animations.contains_key(s))
        {
            self.selected = self.lib.animations.keys().next().cloned();
        }
    }

    /// The canonical text a save writes.
    pub fn text(&self) -> Result<String, UiAnimError> {
        self.lib
            .to_ron_string()
            .map_err(|e| UiAnimError::new("parse", e.to_string()))
    }

    /// Writes the library to `path` (or its own path) through a temp file and a rename. The text is
    /// parsed back with the runtime's loader first, so a file the loader would refuse is never written.
    pub fn save(&mut self, path: Option<PathBuf>) -> Result<PathBuf, UiAnimError> {
        let path = path
            .or_else(|| self.path.clone())
            .ok_or_else(|| UiAnimError::new("io", "the document has no path yet: pass one"))?;
        let text = self.text()?;
        let back = UiAnimLibrary::from_ron(&text).map_err(|e| UiAnimError::new("parse", e.to_string()))?;
        if back != self.lib {
            return Err(UiAnimError::new(
                "parse",
                "written text loads as a different library",
            ));
        }
        let io = |e: std::io::Error| UiAnimError::new("io", format!("{}: {e}", path.display()));
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("lib.uianim.ron");
        let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, &text).map_err(io)?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io(e)
        })?;
        self.saved = self.lib.clone();
        self.path = Some(path.clone());
        Ok(path)
    }

    /// The values of animation `name` at `time` seconds, from [`UiAnimDef::compile`] and
    /// [`TweenPlan::sample`](regia_ui::tween::TweenPlan::sample). Pure: the document does not change.
    pub fn sample(
        &self,
        name: &str,
        time: f32,
        reverse: bool,
        speed: Option<f32>,
    ) -> Result<Vec<(TweenProperty, TweenValue)>, UiAnimError> {
        let _span = tracing::debug_span!(target: "regia_editor", "ui_anim_scrub").entered();
        let def =
            self.lib.animations.get(name).ok_or_else(|| {
                UiAnimError::new("unknown_animation", format!("no animation named `{name}`"))
            })?;
        sample_def(def, time, reverse, speed)
    }
}

/// [`UiAnimDoc::sample`] for a definition: one entry per property, in documentation order.
pub fn sample_def(
    def: &UiAnimDef,
    time: f32,
    reverse: bool,
    speed: Option<f32>,
) -> Result<Vec<(TweenProperty, TweenValue)>, UiAnimError> {
    if !time.is_finite() || time < 0.0 {
        return Err(UiAnimError::new(
            "bad_value",
            "`time` must be a non-negative number of seconds",
        ));
    }
    let plan = def
        .compile(reverse, speed)
        .map_err(|e| UiAnimError::new("invalid", e.to_string()))?;
    let mut raw = Vec::new();
    plan.sample(time, &mut raw);
    let mut out = Vec::new();
    for property in TweenProperty::ALL {
        if let Some((_, v)) = raw.iter().rev().find(|(p, _)| *p == property) {
            out.push((property, *v));
        }
    }
    Ok(out)
}

/// A sample as JSON: `{ property: number | [r, g, b, a] }`.
#[must_use]
pub fn values_json(values: &[(TweenProperty, TweenValue)]) -> Value {
    let mut map = serde_json::Map::new();
    for (p, v) in values {
        let v = match v {
            TweenValue::Number(n) => json!(n),
            TweenValue::Color(c) => json!(c.0),
        };
        map.insert(p.name().to_owned(), v);
    }
    Value::Object(map)
}

/// The node facts the validator checks a library against: every named node of `file`.
#[must_use]
pub fn node_facts(file: &UiFile) -> Vec<UiNodeFact> {
    let mut out = Vec::new();
    walk(&file.root, &mut Vec::new(), &mut |node, _| {
        if let Some(name) = &node.name {
            out.push(UiNodeFact {
                name: name.clone(),
                has_text: node.text.is_some(),
            });
        }
    });
    out
}

/// A copy of `file` with the sampled `values` applied to the node named `target`, for the designer's
/// preview. Returns the copy and the properties the static layout cannot show (`translate_x`,
/// `translate_y`, `scale` are runtime transforms; `text_color` is left to the runtime). `None` when no
/// node has that name.
#[must_use]
pub fn apply_to_file(
    file: &UiFile,
    target: &str,
    values: &[(TweenProperty, TweenValue)],
) -> Option<(UiFile, Vec<&'static str>)> {
    fn find<'a>(node: &'a mut UiNodeDef, name: &str) -> Option<&'a mut UiNodeDef> {
        if node.name.as_deref() == Some(name) {
            return Some(node);
        }
        node.children.iter_mut().find_map(|c| find(c, name))
    }
    let mut out = file.clone();
    let node = find(&mut out.root, target)?;
    let mut skipped = Vec::new();
    for (property, value) in values {
        match (property, value) {
            (TweenProperty::Opacity, TweenValue::Number(n)) => node.opacity = Some(*n),
            (TweenProperty::Background, TweenValue::Color(c)) => {
                node.background = Some(BackgroundDef::Color(*c))
            }
            (TweenProperty::BorderColor, TweenValue::Color(c)) => {
                if let Some(border) = &mut node.border {
                    border.color = *c;
                }
            }
            (TweenProperty::Width, TweenValue::Number(n)) => node.style.size[0] = Dimension::Points(*n),
            (TweenProperty::Height, TweenValue::Number(n)) => node.style.size[1] = Dimension::Points(*n),
            (TweenProperty::InsetLeft, TweenValue::Number(n)) => {
                node.style.inset.left = Dimension::Points(*n)
            }
            (TweenProperty::InsetTop, TweenValue::Number(n)) => node.style.inset.top = Dimension::Points(*n),
            (TweenProperty::InsetRight, TweenValue::Number(n)) => {
                node.style.inset.right = Dimension::Points(*n)
            }
            (TweenProperty::InsetBottom, TweenValue::Number(n)) => {
                node.style.inset.bottom = Dimension::Points(*n);
            }
            (p, _) => skipped.push(p.name()),
        }
    }
    Some((out, skipped))
}

/// Parses the `ease` of a key from the agent or the panel: a [`Ease::parse`] name.
pub fn check_ease(name: &str) -> Result<(), UiAnimError> {
    Ease::parse(name)
        .map(|_| ())
        .ok_or_else(|| UiAnimError::new("bad_value", format!("unknown ease `{name}`")))
}

/// Limits the panel and the agent show.
pub const LIMITS: (u32, (f32, f32)) = (MAX_LOOPS, SPEED_RANGE);

/// The animations that target node `node`.
#[must_use]
pub fn animations_for<'a>(lib: &'a UiAnimLibrary, node: &str) -> Vec<&'a str> {
    lib.animations
        .keys()
        .filter(|n| target_of(n) == Some(node))
        .map(String::as_str)
        .collect()
}

/// The UI Animation editor: at most one open library, plus the panel's scrub state (none of it is
/// part of the document).
#[derive(Resource, Debug, Default)]
pub struct UiAnimEditor {
    doc: Option<UiAnimDoc>,
    /// The last error the panel shows.
    pub error: Option<String>,
    /// The scrub position in seconds.
    pub time: f32,
    /// Scrub the animation reversed.
    pub reverse: bool,
    /// Scrub is playing in the panel (advanced by the panel's frame clock).
    pub playing: bool,
}

impl UiAnimEditor {
    /// The open document.
    #[must_use]
    pub fn doc(&self) -> Option<&UiAnimDoc> {
        self.doc.as_ref()
    }

    /// The open document, mutably.
    pub fn doc_mut(&mut self) -> Result<&mut UiAnimDoc, UiAnimError> {
        self.doc
            .as_mut()
            .ok_or_else(|| UiAnimError::new("no_document", "no animation library is open"))
    }

    /// Replaces the open document.
    pub fn set_doc(&mut self, doc: UiAnimDoc) {
        self.doc = Some(doc);
        self.error = None;
        self.time = 0.0;
        self.playing = false;
    }
}

/// The next document identity on the shared history.
fn next_serial() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl UiAnimDoc {
    /// What the shared history names the document by.
    fn history_path(&self) -> String {
        self.path.as_ref().map_or_else(
            || "untitled .uianim.ron".to_owned(),
            |path| path.display().to_string().replace('\\', "/"),
        )
    }
}

/// The UI Animation editor's library on the editor's shared history (P-322,
/// `crate::shared_panel_history`).
pub(crate) struct UiAnimHistory;

impl SharedDoc for UiAnimHistory {
    type State = UiAnimLibrary;
    const PANEL: &'static str = "UI animation";

    fn histories(world: &mut World) -> Vec<&mut SharedHistory<UiAnimLibrary>> {
        world
            .get_resource_mut::<UiAnimEditor>()
            .and_then(|editor| editor.into_inner().doc.as_mut())
            .map(|doc| vec![&mut doc.shared_history])
            .unwrap_or_default()
    }

    fn restore(world: &mut World, doc: u64, path: &str, state: &UiAnimLibrary) {
        let Some(open) = world
            .get_resource_mut::<UiAnimEditor>()
            .and_then(|editor| editor.into_inner().doc.as_mut())
            .filter(|open| open.serial == doc)
        else {
            tracing::warn!(target: "regia_editor", path, "UI animation history step for a library that is no longer open");
            return;
        };
        open.lib = state.clone();
        open.revision += 1;
        open.fix_selection();
    }

    fn open_doc(world: &World) -> Option<(u64, String)> {
        world
            .get_resource::<UiAnimEditor>()
            .and_then(UiAnimEditor::doc)
            .map(|doc| (doc.serial, doc.history_path()))
    }
}
