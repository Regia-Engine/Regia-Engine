// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from bevy-inspector-egui 0.37.0
// (https://github.com/jakobhellermann/bevy-inspector-egui, commit ac67298,
// crates/bevy-inspector-egui/src/reflect_inspector/mod.rs),
// Copyright (c) Jakob Hellermann and bevy-inspector-egui contributors,
// licensed MIT OR Apache-2.0; used here under the MIT licence:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! The generic reflection drawer: every [`ReflectMut`] kind — struct, tuple struct, tuple, list,
//! array, map, set, enum (with data-carrying variants and `Option`) — plus leaf widgets from
//! [`InspectorImpls`].
//!
//! Regia Engine's contract on top of upstream: the drawer edits a *clone* of a component and records
//! the reflect path of every edit ([`Change`]); the panel turns each into a
//! [`SetProperty`](crate::command::SetProperty) on the undo stack (undo, agent audit). Structural
//! edits (add/remove/move elements, variant switches) are marked `discrete` so they never
//! coalesce with a neighbouring drag. Values inside maps and sets have no reflect path, so their
//! edits are recorded on the whole collection.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use egui::Grid;
use regia_ecs::attrs::EditorAttrs;
use regia_ecs::reflect::array::Array;
use regia_ecs::reflect::enums::{DynamicEnum, DynamicVariant, Enum, EnumInfo, VariantInfo, VariantType};
use regia_ecs::reflect::list::List;
use regia_ecs::reflect::map::Map;
use regia_ecs::reflect::set::Set;
use regia_ecs::reflect::std_traits::ReflectDefault;
use regia_ecs::reflect::structs::{DynamicStruct, Struct};
use regia_ecs::reflect::tuple::{DynamicTuple, Tuple};
use regia_ecs::reflect::tuple_struct::TupleStruct;
use regia_ecs::reflect::{PartialReflect, Reflect, ReflectFromReflect, ReflectMut, TypeInfo, TypeRegistry};

use super::egui_utils::{
    add_button, down_button, no_default_value, remove_button, unconstructable_variant, up_button,
};
use super::impls::{InspectorEguiImpl, InspectorImpls, color_ui_in};
use super::options::{FieldOptions, ReflectInspectorOptions, struct_field_options, variant_field_options};
use crate::dnd::ContentKind;

/// Whether a field gets the colour picker: only colour-like fields (name or `@ColorHdr`).
pub(crate) fn is_colour_field(field: Option<&FieldOptions>) -> bool {
    field.is_some_and(|f| f.color)
}

/// One edit the drawer made to the inspected value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Reflect path of the edited field inside the component (`""` = the whole component).
    pub path: String,
    /// A structural edit (element added/removed/moved, variant switched): its own undo entry.
    pub discrete: bool,
}

/// What the drawer knows beyond the value: the component it is inside (for unit inference), the
/// project's assets (for pickers), and the edits recorded so far.
#[derive(Debug, Default)]
pub struct InspectorContext {
    /// Type path of the inspected component.
    pub component: String,
    /// Project assets `(relative path, kind)` offered by asset pickers (shared with the
    /// inspector's catalog, so a context per component costs no copy of the list).
    pub assets: std::sync::Arc<[(String, ContentKind)]>,
    path: String,
    changes: Vec<Change>,
    suppress: u32,
    /// Reflect paths whose values differ across the current multi-selection. Empty for one entity.
    /// A vector or Euler lane is its own path (`offset.x`, `rotation.ry`); a colour channel of an
    /// array is `tint[2]`.
    mixed: BTreeSet<String>,
    /// The number a lane widget showed when it recorded its edit (`rotation.ry` -> degrees).
    /// A quaternion's Euler angles have two readings (`ry = 120` is `rx = rz = 180, ry = 60`), so
    /// the commit takes the typed number from here instead of decomposing the edited quaternion.
    lane_values: BTreeMap<String, f32>,
    /// Field paths whose **Add to sequence** context item was picked this frame (ADR-9710).
    sequence_requests: Vec<String>,
    /// The Content Browser's thumbnail cache, lent while the drawer runs so asset pickers show
    /// pictures (ADR-7472).
    thumbs: Option<ThumbSlot>,
    /// The Inspector's search and **Modified only** row filter (`inspector::search`).
    filter: Option<std::sync::Arc<super::search::RowFilter>>,
    /// Reflect paths of this component pinned to the top (`inspector::pinned`).
    pinned: Vec<String>,
    /// **Pin to top** / **Unpin** picks from field context menus this frame: `(path, on)`.
    pin_requests: Vec<(String, bool)>,
    /// Field paths of the inspected component that the prefab instance overrides: each such field
    /// row carries an override marker next to its label (ADR-7744).
    overrides: BTreeSet<String>,
    /// The Revert or Apply picked from a marker's menu this frame: `(field path, action)`.
    override_requests: Vec<(String, OverrideRequest)>,
    /// The overridden field paths whose row drew a marker with the Revert/Apply menu this frame;
    /// every other override still needs a row under the component (ADR-7744).
    marked: BTreeSet<String>,
    /// Field tools (copy/paste, reset, tooltips, raw view; `inspector::clipboard`), when the
    /// panel lent them.
    tools: Option<super::clipboard::FieldTools>,
    /// Field context-menu picks and field shortcuts of this frame (`inspector::clipboard`).
    field_requests: Vec<super::clipboard::FieldRequest>,
    /// `enabled` fields drawn as a header checkbox (`@Toggle`): skipped in the group's body.
    toggle_paths: BTreeSet<String>,
    /// A `@Toggle` header checkbox's new state this frame (taken by the struct drawer).
    toggle_pick: Option<bool>,
    /// A drag over the current field's label this frame (ADR-7785): points moved (right and up
    /// positive) and the modifiers held. A number widget takes it; the row drops what is left.
    label_scrub: Option<(f64, egui::Modifiers)>,
    /// Field paths whose inspector button (`@Button`) was clicked this frame (ADR-7785).
    action_requests: Vec<String>,
    /// Link keys ([`Self::link_key`]) of the vector fields drawn with linked components.
    linked: BTreeSet<String>,
    /// Link toggles clicked this frame: `(link key, linked)`.
    link_toggles: Vec<(String, bool)>,
    /// The open project's root, for file pickers and layer names (`None` without a project).
    pub project_root: Option<std::path::PathBuf>,
    /// What the asset, color and entity pickers read from the editor and ask of it (ADR-7786).
    pub pickers: super::picker_links::PickerLinks,
    /// Row labels by field path for a panel that shows a type without `@DisplayName`s (World
    /// Settings, ADR-8586); a `@DisplayName` still wins.
    field_labels: Option<fn(&str) -> Option<&'static str>>,
}

/// What a prefab-override marker's menu asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideRequest {
    /// Drop the override: the field follows the prefab again.
    Revert,
    /// Write the field's value into the prefab.
    Apply,
}

/// The thumbnail cache an [`InspectorContext`] holds while drawing.
struct ThumbSlot(Box<crate::thumb_cache::ThumbCache>);

impl std::fmt::Debug for ThumbSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ThumbSlot")
    }
}

impl InspectorContext {
    /// A context for drawing (a value inside) `component`.
    #[must_use]
    pub fn new(component: impl Into<String>) -> Self {
        Self {
            component: component.into(),
            ..Self::default()
        }
    }

    /// With row labels by field path (`game_rules` -> `Game Rules`); the field ids stay the reflect
    /// paths of edits, files and agent params (ADR-8586).
    #[must_use]
    pub fn with_field_labels(mut self, labels: fn(&str) -> Option<&'static str>) -> Self {
        self.field_labels = Some(labels);
        self
    }

    /// With the assets offered by asset pickers.
    #[must_use]
    pub fn with_assets(mut self, assets: impl Into<std::sync::Arc<[(String, ContentKind)]>>) -> Self {
        self.assets = assets.into();
        self
    }

    /// With the thumbnail cache asset pickers draw pictures from; take it back with
    /// [`Self::take_thumbnails`] after drawing.
    #[must_use]
    pub fn with_thumbnails(mut self, cache: Option<crate::thumb_cache::ThumbCache>) -> Self {
        self.thumbs = cache.map(|cache| ThumbSlot(Box::new(cache)));
        self
    }

    /// With the field paths a prefab instance overrides (`intensity`, `offset.x`): their rows get an
    /// override marker beside the label. Paths for the whole component (empty, `.`) are not fields.
    #[must_use]
    pub fn with_overrides(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.overrides.extend(
            paths
                .into_iter()
                .map(Into::into)
                .map(|p: String| p.strip_prefix('.').unwrap_or(&p).to_owned())
                .filter(|p| !p.is_empty()),
        );
        self
    }

    /// How the field at `path` relates to the overrides: `Some(true)` the field itself is
    /// overridden, `Some(false)` something inside it is, `None` neither.
    #[must_use]
    pub fn override_mark(&self, path: &str) -> Option<bool> {
        if self.overrides.contains(path) {
            return Some(true);
        }
        self.overrides
            .iter()
            .any(|o| {
                o.strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with(['.', '[']))
            })
            .then_some(false)
    }

    /// Takes the Revert and Apply requests the markers' menus recorded.
    pub fn take_override_requests(&mut self) -> Vec<(String, OverrideRequest)> {
        std::mem::take(&mut self.override_requests)
    }

    /// Takes the overridden field paths that got a marker with the Revert/Apply menu while drawing
    /// (an override inside a list element or a vector lane has no row of its own, so it is not
    /// here and keeps its row under the component).
    pub fn take_marked_overrides(&mut self) -> BTreeSet<String> {
        std::mem::take(&mut self.marked)
    }

    /// With the pickers' snapshot ([`super::picker_links::links`]).
    #[must_use]
    pub fn with_pickers(mut self, pickers: super::picker_links::PickerLinks) -> Self {
        self.pickers = pickers;
        self
    }

    /// Hands the thumbnail cache back.
    pub fn take_thumbnails(&mut self) -> Option<crate::thumb_cache::ThumbCache> {
        self.thumbs.take().map(|slot| *slot.0)
    }

    /// The picture of project-relative `rel`, when the lent cache has one (asked for, never
    /// waited on).
    pub fn thumbnail(&mut self, ctx: &egui::Context, rel: &str) -> Option<egui::TextureId> {
        let cache = &mut self.thumbs.as_mut()?.0;
        if rel.is_empty() {
            return None;
        }
        cache.request(rel);
        cache.texture(ctx, rel)
    }

    /// Paths that differ across the selection (see [`Self::is_mixed`]).
    #[must_use]
    pub fn with_mixed(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.mixed.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Whether the value at the current path differs across the selection.
    #[must_use]
    pub fn is_mixed(&self) -> bool {
        self.mixed.contains(&self.path)
    }

    /// Whether the named child lane (`x`, `rx`, …) differs across the selection.
    #[must_use]
    pub fn lane_mixed(&self, lane: &str) -> bool {
        self.mixed.contains(&self.child_field(lane))
    }

    /// Whether array or list index `index` differs across the selection.
    #[must_use]
    pub fn index_mixed(&self, index: usize) -> bool {
        self.mixed.contains(&format!("{}[{index}]", self.path))
    }

    /// The reflect path of the value being drawn.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The edits recorded so far.
    #[must_use]
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// Takes the recorded edits.
    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    /// Takes the field paths whose **Add to sequence** item was picked (the inspector keys them
    /// into the open sequence, ADR-9710).
    pub fn take_sequence_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.sequence_requests)
    }

    /// Records an edit of the value at the current path.
    pub fn record(&mut self, discrete: bool) {
        if self.suppress > 0 {
            return;
        }
        if let Some(existing) = self.changes.iter_mut().find(|c| c.path == self.path) {
            existing.discrete |= discrete;
            return;
        }
        self.changes.push(Change {
            path: self.path.clone(),
            discrete,
        });
    }

    /// Records an edit of the child lane `lane` (`.x`, `.ry`) and leaves the current path as it was.
    ///
    /// The parent widget then does not also record the whole value, so one axis can change while
    /// the others stay per entity.
    pub fn record_lane(&mut self, lane: &str) {
        let len = self.path.len();
        if !self.path.is_empty() {
            self.path.push('.');
        }
        self.path.push_str(lane);
        self.record(false);
        self.path.truncate(len);
    }

    /// Records an edit of the child lane `lane` like [`Self::record_lane`], and the number the
    /// widget shows for it, so a commit writes that number rather than another reading of the
    /// same rotation (an Euler `ry` past 90 degrees).
    pub fn record_lane_value(&mut self, lane: &str, value: f32) {
        if self.suppress > 0 {
            return;
        }
        let path = self.child_field(lane);
        self.lane_values.insert(path, value);
        self.record_lane(lane);
    }

    /// The numbers recorded with [`Self::record_lane_value`], by full lane path.
    #[must_use]
    pub fn lane_values(&self) -> &BTreeMap<String, f32> {
        &self.lane_values
    }

    /// Records an edit of `[index]` and leaves the current path as it was.
    pub fn record_index(&mut self, index: usize) {
        let len = self.path.len();
        let _ = write!(self.path, "[{index}]");
        self.record(false);
        self.path.truncate(len);
    }

    /// With the Inspector's row filter (search, **Modified only**); rows it rejects are not drawn.
    #[must_use]
    pub fn with_filter(mut self, filter: Option<super::search::RowFilter>) -> Self {
        self.filter = filter.map(std::sync::Arc::new);
        self
    }

    /// With the component's pinned field paths (the field menu offers **Unpin** on them).
    #[must_use]
    pub fn with_pinned(mut self, pinned: Vec<String>) -> Self {
        self.pinned = pinned;
        self
    }

    /// Takes the **Pin to top** / **Unpin** picks of this frame: `(path, on)`.
    pub fn take_pin_requests(&mut self) -> Vec<(String, bool)> {
        std::mem::take(&mut self.pin_requests)
    }

    /// Notes a **Pin to top** (`on`) or **Unpin** pick for the field at reflect `path`.
    pub fn request_pin(&mut self, path: &str, on: bool) {
        self.pin_requests.push((path.to_owned(), on));
    }

    /// Whether the row at reflect `path` passes the row filter (always, without one).
    #[must_use]
    pub fn row_visible(&self, path: &str) -> bool {
        self.filter.as_ref().is_none_or(|filter| filter.row_visible(path))
    }

    /// Whether a row filter is on.
    #[must_use]
    pub fn filtering(&self) -> bool {
        self.filter.is_some()
    }

    /// With the field tools (baseline, modified paths, raw view, entity) of `inspector::clipboard`.
    #[must_use]
    pub fn with_field_tools(mut self, tools: super::clipboard::FieldTools) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Takes the field tools back after a draw (the Inspector keeps them for the next repaint).
    pub fn take_field_tools(&mut self) -> Option<super::clipboard::FieldTools> {
        self.tools.take()
    }

    /// Takes the field context-menu picks and field shortcuts of this frame.
    pub fn take_field_requests(&mut self) -> Vec<super::clipboard::FieldRequest> {
        std::mem::take(&mut self.field_requests)
    }

    /// Whether the raw view is on (hidden fields shown read-only).
    #[must_use]
    pub fn raw(&self) -> bool {
        self.tools.as_ref().is_some_and(|tools| tools.raw)
    }

    /// Whether the field at reflect `path` differs from its baseline (it or a field inside it).
    #[must_use]
    pub fn is_modified(&self, path: &str) -> bool {
        self.tools.as_ref().is_some_and(|tools| tools.is_modified(path))
    }

    /// Whether edits are being recorded (false inside a draft, a read-only field or a matrix
    /// column).
    #[must_use]
    pub fn recording(&self) -> bool {
        self.suppress == 0
    }

    /// Takes this frame's label drag for the current field (ADR-7785).
    pub fn take_label_scrub(&mut self) -> Option<(f64, egui::Modifiers)> {
        self.label_scrub.take()
    }

    /// Sets the label drag the next number widget takes (the field row does this; tests too).
    pub fn set_label_scrub(&mut self, scrub: Option<(f64, egui::Modifiers)>) {
        self.label_scrub = scrub;
    }

    /// Asks for the inspector button at the current path to run (ADR-7785).
    pub fn request_action(&mut self) {
        if self.suppress == 0 {
            self.action_requests.push(self.path.clone());
        }
    }

    /// Takes the field paths whose inspector button was clicked.
    pub fn take_action_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.action_requests)
    }

    /// The key a vector field's link state is remembered under: `<Component>.<path>` with the
    /// component's short name (`Transform.scale`).
    #[must_use]
    pub fn link_key(&self) -> String {
        let component = self.component.rsplit("::").next().unwrap_or(&self.component);
        format!("{component}.{}", self.path)
    }

    /// With the link keys of the vector fields whose components are linked.
    #[must_use]
    pub fn with_linked(mut self, keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.linked.extend(keys.into_iter().map(Into::into));
        self
    }

    /// Whether the vector field at the current path has its components linked.
    #[must_use]
    pub fn is_linked(&self) -> bool {
        self.linked.contains(&self.link_key())
    }

    /// Flips the link of the vector field at the current path (shown at once; the inspector
    /// stores it in the user's preferences).
    pub fn toggle_link(&mut self) {
        let key = self.link_key();
        let linked = !self.linked.remove(&key);
        if linked {
            self.linked.insert(key.clone());
        }
        self.link_toggles.push((key, linked));
    }

    /// Takes the link toggles clicked this frame.
    pub fn take_link_toggles(&mut self) -> Vec<(String, bool)> {
        std::mem::take(&mut self.link_toggles)
    }

    fn child_field(&self, name: &str) -> String {
        if self.path.is_empty() {
            name.to_string()
        } else {
            format!("{}.{name}", self.path)
        }
    }
}

/// What `InspectorUi::struct_rows` needs of the struct being drawn.
struct StructParts<'a> {
    /// The struct's type info.
    info: &'a regia_ecs::reflect::structs::StructInfo,
    /// Each field's attribute-derived options.
    derived: &'a [FieldOptions],
    /// The struct's own options.
    options: &'a dyn Any,
    /// The struct's widget id.
    id: egui::Id,
}

/// What a field row knows beyond its options: its doc comment, its `@EditCondition` state and a
/// `@Toggle` group's `enabled`.
#[derive(Debug, Clone, Copy, Default)]
struct RowInfo<'a> {
    /// The field's doc comment.
    doc: Option<&'a str>,
    /// The field's `@EditCondition`, evaluated.
    condition: Option<ConditionState>,
    /// A `@Toggle` group's `enabled` (drawn as the row's header checkbox).
    toggle: Option<bool>,
}

/// An evaluated `@EditCondition`.
#[derive(Debug, Clone, Copy)]
struct ConditionState {
    /// The condition's text.
    source: &'static str,
    /// Whether it holds (a condition that does not parse holds).
    holds: bool,
    /// `@EditConditionHides`.
    hides: bool,
}

impl ConditionState {
    /// The condition of a field with `attrs`, evaluated against `parent` (the struct holding it).
    fn of(attrs: &EditorAttrs, parent: &dyn PartialReflect) -> Option<Self> {
        let source = attrs.edit_condition?;
        Some(Self {
            source,
            holds: super::condition::evaluate(source, parent).unwrap_or(true),
            hides: attrs.edit_condition_hides,
        })
    }
}

/// `enabled` of a `@Toggle` group (a struct with an `enabled: bool` field).
fn toggle_state(attrs: &EditorAttrs, value: &dyn PartialReflect) -> Option<bool> {
    if !attrs.toggle {
        return None;
    }
    match value.reflect_ref() {
        regia_ecs::reflect::ReflectRef::Struct(group) => group
            .field("enabled")
            .and_then(|enabled| enabled.try_downcast_ref::<bool>())
            .copied(),
        _ => None,
    }
}

/// One step of a reflect path.
#[derive(Debug, Clone, Copy)]
enum Segment<'a> {
    /// `.name` (or `.0` for tuple fields).
    Field(&'a str),
    /// `[i]`.
    Index(usize),
}

/// An add/remove/move on one list element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOp {
    /// Insert a new element (the item type's default, else a copy of the neighbour) at the index.
    AddElement(usize),
    /// Remove the element at the index.
    RemoveElement(usize),
    /// Swap the element with the one before it.
    MoveElementUp(usize),
    /// Swap the element with the one after it.
    MoveElementDown(usize),
    /// Insert a copy of the element right after it.
    DuplicateElement(usize),
    /// Move the element at `from` so it ends at index `to` (a drag of its handle).
    MoveElement {
        /// Index of the element to move.
        from: usize,
        /// Index it ends at.
        to: usize,
    },
    /// Remove every element.
    Clear,
}

thread_local! {
    static CLONE_CONCRETE_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A field label's context menu, also reachable from the keyboard: Shift+F10 while the label has
/// the keyboard focus opens it under the label, and Tab then walks its items (review
/// sprint-v06-rev-late-k #24: Pin to top was mouse-only). A right-click opens it at the pointer.
fn field_context_menu(response: &egui::Response, add: impl FnOnce(&mut egui::Ui)) {
    let ctx = &response.ctx;
    let id = egui::Popup::default_response_id(response);
    let by_key =
        response.has_focus() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::F10));
    let mut popup = egui::Popup::context_menu(response);
    if by_key {
        popup = popup.open_memory(egui::SetOpenCommand::Bool(true));
    }
    // A menu opened from the keyboard has no pointer position to sit at: it opens under the label.
    if by_key || (egui::Popup::is_id_open(ctx, id) && egui::Popup::position_of_id(ctx, id).is_none()) {
        popup = popup.at_position(response.rect.left_bottom());
    }
    let _ = popup.show(add);
}

/// How many times [`clone_concrete`] ran on the calling thread: a counter for tests and benches
/// (a repaint of an unchanged component must not clone it again, ADR-7326).
#[must_use]
pub fn clone_concrete_calls() -> u64 {
    CLONE_CONCRETE_CALLS.with(std::cell::Cell::get)
}

/// A clone of a reflected value that keeps its concrete type when the type supports it.
pub fn clone_value(value: &dyn PartialReflect) -> Option<Box<dyn PartialReflect>> {
    match value.reflect_clone() {
        Ok(concrete) => Some(concrete.into_partial_reflect()),
        Err(_) => value.to_dynamic().ok(),
    }
}

/// Like [`clone_value`], but a dynamic fallback is turned back into the concrete type through its
/// registered `ReflectFromReflect`. bevy_reflect 0.20 has no `reflect_clone` for fixed-size arrays,
/// so a component holding a `[f32; 3]` clones dynamic, and type-keyed widgets (colours, glam
/// vectors, `InspectorEguiImpl`s) would never match its fields.
pub fn clone_concrete(
    registry: &TypeRegistry,
    value: &dyn PartialReflect,
) -> Option<Box<dyn PartialReflect>> {
    CLONE_CONCRETE_CALLS.with(|calls| calls.set(calls.get() + 1));
    let cloned = clone_value(value)?;
    if !cloned.is_dynamic() {
        return Some(cloned);
    }
    let concrete = cloned
        .get_represented_type_info()
        .and_then(|info| registry.get_type_data::<ReflectFromReflect>(info.type_id()))
        .and_then(|from| from.from_reflect(cloned.as_ref()));
    match concrete {
        Some(concrete) => Some(concrete.into_partial_reflect()),
        None => Some(cloned),
    }
}

/// A new value of a registered type from its `ReflectDefault`.
#[must_use]
pub fn default_value(registry: &TypeRegistry, type_id: TypeId) -> Option<Box<dyn Reflect>> {
    registry
        .get_type_data::<ReflectDefault>(type_id)
        .map(ReflectDefault::default)
}

/// Applies a [`ListOp`] to a list (the list buttons' effect).
///
/// # Errors
/// When a new element is needed but the item type has no default and the list no neighbour to
/// copy, or the index is out of range.
pub fn apply_list_op(registry: &TypeRegistry, list: &mut dyn List, op: ListOp) -> Result<(), String> {
    let len = list.len();
    match op {
        ListOp::AddElement(i) => {
            let item_type = match list.get_represented_type_info() {
                Some(TypeInfo::List(info)) => Some(info.item_ty().id()),
                _ => None,
            };
            let new_value = item_type
                .and_then(|id| default_value(registry, id))
                .map(|value| value.into_partial_reflect())
                .or_else(|| list.get(i.min(len.saturating_sub(1))).and_then(clone_value))
                .ok_or_else(|| {
                    "the item type has no #[reflect(Default)] and the list is empty".to_string()
                })?;
            list.insert(i.min(len), new_value);
        }
        ListOp::RemoveElement(i) => {
            if i >= len {
                return Err(format!("index {i} out of range (len {len})"));
            }
            list.remove(i);
        }
        ListOp::MoveElementUp(i) => {
            if i == 0 || i >= len {
                return Err(format!("cannot move element {i} up (len {len})"));
            }
            let element = list.remove(i);
            list.insert(i - 1, element);
        }
        ListOp::MoveElementDown(i) => {
            if i + 1 >= len {
                return Err(format!("cannot move element {i} down (len {len})"));
            }
            let element = list.remove(i);
            list.insert(i + 1, element);
        }
        ListOp::DuplicateElement(i) => {
            let copy = list
                .get(i)
                .ok_or_else(|| format!("index {i} out of range (len {len})"))?;
            let copy =
                clone_concrete(registry, copy).ok_or_else(|| format!("element {i} cannot be copied"))?;
            list.insert(i + 1, copy);
        }
        ListOp::MoveElement { from, to } => {
            if from >= len || to >= len {
                return Err(format!("cannot move element {from} to {to} (len {len})"));
            }
            if from != to {
                let element = list.remove(from);
                list.insert(to, element);
            }
        }
        ListOp::Clear => {
            drop(list.drain());
        }
    }
    Ok(())
}

/// A default-constructed value of one enum variant: every field from its type's
/// `ReflectDefault`.
///
/// # Errors
/// The type paths of the fields that have no default.
pub fn default_variant(registry: &TypeRegistry, variant: &VariantInfo) -> Result<DynamicEnum, Vec<String>> {
    let missing = |type_id: TypeId| registry.get_type_data::<ReflectDefault>(type_id).is_none();
    let dynamic_variant = match variant {
        VariantInfo::Struct(info) => {
            let unconstructable: Vec<String> = info
                .iter()
                .filter(|f| missing(f.ty().id()))
                .map(|f| f.type_path().to_string())
                .collect();
            if !unconstructable.is_empty() {
                return Err(unconstructable);
            }
            let mut dynamic = DynamicStruct::default();
            for field in info.iter() {
                if let Some(value) = default_value(registry, field.ty().id()) {
                    dynamic.insert_boxed(field.name(), value.into_partial_reflect());
                }
            }
            DynamicVariant::Struct(dynamic)
        }
        VariantInfo::Tuple(info) => {
            let unconstructable: Vec<String> = info
                .iter()
                .filter(|f| missing(f.ty().id()))
                .map(|f| f.type_path().to_string())
                .collect();
            if !unconstructable.is_empty() {
                return Err(unconstructable);
            }
            let mut dynamic = DynamicTuple::default();
            for field in info.iter() {
                if let Some(value) = default_value(registry, field.ty().id()) {
                    dynamic.insert_boxed(value.into_partial_reflect());
                }
            }
            DynamicVariant::Tuple(dynamic)
        }
        VariantInfo::Unit(_) => DynamicVariant::Unit,
    };
    Ok(DynamicEnum::new(variant.name(), dynamic_variant))
}

/// Switches an enum value to the named variant with default field values (the variant combo's
/// effect). Switching to the active variant is a no-op.
///
/// # Errors
/// When the variant does not exist, its fields have no defaults, or the value rejects it.
pub fn switch_variant(registry: &TypeRegistry, value: &mut dyn Enum, variant: &str) -> Result<(), String> {
    if value.variant_name() == variant {
        return Ok(());
    }
    let Some(info) = value.get_represented_enum_info() else {
        return Err("the enum has no type info".into());
    };
    let Some(variant_info) = info.variant(variant) else {
        return Err(format!("{} has no variant {variant}", info.type_path()));
    };
    let dynamic = default_variant(registry, variant_info)
        .map_err(|fields| format!("{variant} has fields without a default: {}", fields.join(", ")))?;
    value.try_apply(&dynamic).map_err(|e| e.to_string())
}

/// The drawer. Cheap to build per frame; [`reborrow`](Self::reborrow) hands it to nested widgets.
pub struct InspectorUi<'a> {
    /// The type registry the values are registered in.
    pub type_registry: &'a TypeRegistry,
    /// The leaf widget table.
    pub impls: &'a InspectorImpls,
    /// Component, assets and recorded edits.
    pub context: &'a mut InspectorContext,
}

impl<'a> InspectorUi<'a> {
    /// A drawer over `type_registry` with the built-in widgets.
    pub fn new(type_registry: &'a TypeRegistry, context: &'a mut InspectorContext) -> Self {
        Self {
            type_registry,
            impls: InspectorImpls::global(),
            context,
        }
    }

    /// The same drawer with a shorter borrow.
    pub fn reborrow(&mut self) -> InspectorUi<'_> {
        InspectorUi {
            type_registry: self.type_registry,
            impls: self.impls,
            context: self.context,
        }
    }

    /// The spoken name of the value being drawn (FU-6, `docs/evidence/editor-a11y-2026-11.md`):
    /// the last segment of its reflect path (`visible`, `tags[1]`), or the component's name at
    /// the root. A leaf widget whose visible label is a separate widget (the boolean checkbox)
    /// announces itself with it.
    #[must_use]
    pub fn field_name(&self) -> String {
        let path = self.context.path();
        match path.rsplit_once('.') {
            Some((_, last)) if !last.is_empty() => last.to_owned(),
            _ if path.is_empty() => self.context.component.clone(),
            _ => path.to_owned(),
        }
    }

    /// Draws `value`; returns whether it changed.
    pub fn ui_for_reflect(&mut self, value: &mut dyn PartialReflect, ui: &mut egui::Ui) -> bool {
        self.ui_for_reflect_with_options(value, ui, egui::Id::new("inspector"), &())
    }

    /// Draws `value` with options: [`InspectorOptions`](super::options::InspectorOptions) for structs/enums, or a leaf's options
    /// ([`NumberOptions`](super::options::NumberOptions), [`FieldOptions`], ...).
    pub fn ui_for_reflect_with_options(
        &mut self,
        value: &mut dyn PartialReflect,
        ui: &mut egui::Ui,
        id: egui::Id,
        options: &dyn Any,
    ) -> bool {
        let before = self.context.changes.len();
        let changed = self.dispatch(value, ui, id, options);
        if changed && self.context.changes.len() == before {
            self.context.record(false);
        }
        changed
    }

    fn dispatch(
        &mut self,
        value: &mut dyn PartialReflect,
        ui: &mut egui::Ui,
        id: egui::Id,
        options: &dyn Any,
    ) -> bool {
        let mut options = options;
        let registered;
        if options.is::<()>()
            && let Some(data) = value.try_as_reflect().and_then(|v| {
                self.type_registry
                    .get_type_data::<ReflectInspectorOptions>(v.as_any().type_id())
            })
        {
            registered = data.0.clone();
            options = &registered;
        }

        // Regia Engine: colours get the picker before any vector widget.
        // Only colour-like fields (name or `@ColorHdr`): a bare `[f32; 3]` such as a light
        // direction or a viewport rect must keep its numeric widget.
        let field = options.downcast_ref::<FieldOptions>();
        if is_colour_field(field) {
            let hdr = field.is_some_and(|f| f.attrs.color_hdr);
            if let Some(changed) = color_ui_in(value, ui, hdr, true, Some(self.context)) {
                return changed;
            }
        }

        // Curve and gradient key lists draw a mini graph with a popover editor (ADR-7786).
        if let Some(changed) =
            super::curve_drawer::curve_field_ui(value, ui, id, self.type_registry, self.context)
        {
            return changed;
        }

        if let Some(value) = value.try_as_reflect_mut() {
            let type_id = value.as_any().type_id();
            let widget: Option<InspectorEguiImpl> = self
                .type_registry
                .get_type_data::<InspectorEguiImpl>(type_id)
                .or_else(|| self.impls.get(type_id))
                .copied();
            if let Some(widget) = widget {
                return widget.execute(value.as_any_mut(), ui, options, id, self.reborrow());
            }
        }

        match value.reflect_mut() {
            ReflectMut::Struct(value) => self.ui_for_struct(value, ui, id, options),
            ReflectMut::TupleStruct(value) => self.ui_for_tuple_struct(value, ui, id, options),
            ReflectMut::Tuple(value) => self.ui_for_tuple(value, ui, id, options),
            ReflectMut::List(value) => self.ui_for_list(value, ui, id, options),
            ReflectMut::Array(value) => self.ui_for_array(value, ui, id, options),
            ReflectMut::Map(value) => self.ui_for_map(value, ui, id),
            ReflectMut::Set(value) => self.ui_for_set(value, ui, id, options),
            ReflectMut::Enum(value) => self.ui_for_enum(value, ui, id, options),
            ReflectMut::Opaque(value) => {
                super::egui_utils::reflect_value_no_impl(ui, value.reflect_type_path());
                false
            }
            #[allow(unreachable_patterns)]
            _ => {
                ui.label("unsupported");
                false
            }
        }
    }

    /// Runs `f` with `segment` appended to the current path.
    fn with_segment<R>(&mut self, segment: Segment<'_>, f: impl FnOnce(&mut Self) -> R) -> R {
        let len = self.context.path.len();
        match segment {
            Segment::Field(name) => {
                if len > 0 {
                    self.context.path.push('.');
                }
                self.context.path.push_str(name);
            }
            Segment::Index(index) => {
                let _ = write!(self.context.path, "[{index}]");
            }
        }
        let result = f(self);
        self.context.path.truncate(len);
        result
    }

    /// Runs `f` without recording edits (drafts, read-only fields, values without a path).
    pub(super) fn suppressed<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.context.suppress += 1;
        let result = f(self);
        self.context.suppress -= 1;
        result
    }

    /// Draws `value` (the field at reflect `path` of the component) with its edits recorded at
    /// `path`: a pinned field drawn out of its place (`inspector::pinned`).
    pub(crate) fn ui_at_path(
        &mut self,
        value: &mut dyn PartialReflect,
        path: &str,
        ui: &mut egui::Ui,
        id: egui::Id,
        options: &dyn Any,
    ) -> bool {
        let saved = std::mem::replace(&mut self.context.path, path.to_owned());
        let changed = self.ui_for_reflect_with_options(value, ui, id, options);
        self.context.path = saved;
        changed
    }

    /// Draws one field: label (display name, tooltip), hidden/read-only handling, the field
    /// context menu (copy/paste, reset, agent path; `inspector::clipboard`), the reset arrow,
    /// `@EditCondition` greying and a `@Toggle` header checkbox.
    fn field_row(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        field_options: &dyn Any,
        row: RowInfo<'_>,
        draw: impl FnOnce(&mut Self, &mut egui::Ui) -> bool,
    ) -> Option<bool> {
        let attrs = field_options.downcast_ref::<FieldOptions>().map(|f| &f.attrs);
        let raw = self.context.raw();
        let hidden = attrs.is_some_and(|a| a.hidden);
        // The raw view shows @Hidden fields, read-only and marked (P-120).
        if hidden && !raw {
            return None;
        }
        let field_path = self.context.child_field(label);
        // A @Toggle group's `enabled` is drawn as the group's header checkbox.
        if self.context.toggle_paths.contains(&field_path) {
            return None;
        }
        // @EditCondition: false greys the field, or hides it with @EditConditionHides (P-119).
        let blocked = row.condition.filter(|c| !c.holds);
        if blocked.is_some_and(|c| c.hides) && !raw {
            return None;
        }
        // The Inspector's search and Modified only (inspector::search).
        if !self.context.row_visible(&field_path) {
            return None;
        }
        let shown = attrs
            .and_then(|a| a.display_name)
            .or_else(|| self.context.field_labels.and_then(|labels| labels(&field_path)))
            .unwrap_or(label);
        // `@Button("Bake")` (ADR-7785): the field is a button that runs the component action
        // named after the field; it has no value to edit.
        if let Some(button) = attrs.and_then(|a| a.button) {
            let path = self.context.child_field(label);
            ui.label("");
            let enabled = !attrs.is_some_and(|a| a.read_only);
            let mut response = ui.add_enabled(enabled, egui::Button::new(button));
            if let Some(tooltip) = attrs.and_then(|a| a.tooltip) {
                response = response.on_hover_text(tooltip);
            }
            if response.clicked() && self.context.suppress == 0 {
                self.context.action_requests.push(path);
            }
            ui.end_row();
            return Some(false);
        }
        let mut text = match self
            .context
            .filter
            .as_ref()
            .and_then(|f| f.highlight(&field_path))
        {
            Some(positions) => super::search::highlighted(ui, shown, positions),
            None => shown.into(),
        };
        if hidden {
            text = format!("{shown} (hidden)").into();
        }
        let modified = self.context.is_modified(&field_path);
        let read_only = attrs.is_some_and(|a| a.read_only) || hidden || blocked.is_some();
        // The header checkbox's pick, handed to the struct drawer after the body is drawn (the
        // body's own rows must not take it).
        let mut picked = None;
        let label_cell = ui.horizontal(|ui| {
            // @Toggle: the group's `enabled` as a checkbox in its header (P-121).
            if let Some(on) = row.toggle {
                let mut on_now = on;
                let spoken = format!("Enable {shown}");
                let response = crate::agent_parity::action_bare_checkbox(
                    ui,
                    &crate::agent_parity::INSPECTOR_TOGGLE_GROUP,
                    &mut on_now,
                    &spoken,
                );
                if response.changed() && !read_only {
                    picked = Some(on_now);
                }
            }
            // A frameless button rather than a label: egui keeps labels out of the Tab order, and
            // the field shortcuts (Ctrl+C / Ctrl+V) need the label to take keyboard focus.
            let response = ui.add(
                egui::Button::new(text)
                    .frame(false)
                    .sense(egui::Sense::click_and_drag()),
            );
            // A drag over the label scrubs a number field like a drag over its body (ADR-7785).
            self.context.label_scrub = None;
            if response.dragged() {
                let delta = response.drag_delta();
                let points = f64::from(delta.x - delta.y);
                if points != 0.0 && !read_only {
                    self.context.label_scrub = Some((points, ui.input(|i| i.modifiers)));
                }
                ui.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            // A prefab instance's override of this field (or of something inside it) is marked beside
            // the label, not under the component (ADR-7744).
            if let Some(exact) = self.context.override_mark(&field_path) {
                self.override_marker(ui, &field_path, exact);
            }
            // The reset arrow beside a field that differs from its default (P-114).
            if modified && !read_only {
                let reset = crate::agent_parity::action_button_text(
                    ui,
                    &crate::agent_parity::INSPECTOR_RESET_FIELD,
                    egui::RichText::new("\u{21ba}").small(),
                );
                let spoken = format!("Reset {shown} to default");
                if crate::a11y_audit::name(reset, egui::WidgetType::Button, &spoken).clicked() {
                    self.context
                        .field_requests
                        .push(super::clipboard::FieldRequest::Reset(field_path.clone()));
                }
            }
            response
        });
        let mut response = label_cell.inner;
        crate::about::HoveredField::record(&response, &self.context.component, &field_path);
        // Tooltip: doc, default value, agent path (P-032); built only while shown.
        if response.hovered() || response.has_focus() {
            let tip = self.field_tooltip(&field_path, attrs, row.doc, blocked);
            response = response.on_hover_ui(|ui| tip.ui(ui));
        }
        self.field_menu(&response, &field_path, modified && !read_only, !read_only);
        let changed = if read_only {
            ui.add_enabled_ui(false, |ui| self.suppressed(|this| draw(this, ui)));
            false
        } else {
            draw(self, ui)
        };
        self.context.label_scrub = None;
        ui.end_row();
        if picked.is_some() {
            self.context.toggle_pick = picked;
        }
        Some(changed)
    }

    /// The override marker beside a field's label: a dot (filled when the field itself is overridden,
    /// hollow when only something inside it is) whose menu reverts the override or applies it to the
    /// prefab.
    fn override_marker(&mut self, ui: &mut egui::Ui, field_path: &str, exact: bool) {
        let accent = ui.visuals().selection.bg_fill;
        let (glyph, hint) = if exact {
            (
                "●",
                "Overridden in this prefab instance. Right-click for Revert and Apply.",
            )
        } else {
            (
                "○",
                "Something inside this field is overridden in this prefab instance.",
            )
        };
        let marker = ui
            .add(egui::Label::new(egui::RichText::new(glyph).color(accent)).sense(egui::Sense::click()))
            .on_hover_text(hint);
        if exact {
            self.context.marked.insert(field_path.to_owned());
            marker.context_menu(|ui| {
                if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::INSPECTOR_OVERRIDE_REVERT)
                    .clicked()
                {
                    self.context
                        .override_requests
                        .push((field_path.to_owned(), OverrideRequest::Revert));
                    ui.close();
                }
                if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::INSPECTOR_OVERRIDE_APPLY)
                    .clicked()
                {
                    self.context
                        .override_requests
                        .push((field_path.to_owned(), OverrideRequest::Apply));
                    ui.close();
                }
            });
        }
    }

    /// The tooltip the field row at `field_path` shows (doc, default, agent path); for tests,
    /// which cannot hover long enough for egui to show it.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn tooltip_for(
        &self,
        field_path: &str,
        attrs: Option<&EditorAttrs>,
        doc: Option<&str>,
    ) -> super::tooltip::FieldTooltip {
        self.field_tooltip(field_path, attrs, doc, None)
    }

    /// The field context menu (right-click a field's label) and the same picks from the keyboard
    /// while the label has focus: Ctrl+C copies the value, Ctrl+Shift+C the agent path, Ctrl+V
    /// pastes (`inspector::clipboard`).
    fn field_menu(&mut self, response: &egui::Response, field_path: &str, can_reset: bool, editable: bool) {
        use super::clipboard::FieldRequest;
        use crate::agent_parity::{
            INSPECTOR_COPY_AGENT_PATH, INSPECTOR_COPY_RON, INSPECTOR_COPY_VALUE, INSPECTOR_PASTE_VALUE,
            INSPECTOR_RESET_FIELD, action_button_text, action_enabled,
        };
        let path = field_path.to_owned();
        let pinned = self.context.pinned.contains(&path);
        let mut picks: Vec<FieldRequest> = Vec::new();
        // Right-click, or Shift+F10 while the label has the keyboard focus (A(d) #24).
        field_context_menu(response, |ui| {
            if action_button_text(ui, &INSPECTOR_COPY_VALUE, "Copy value").clicked() {
                picks.push(FieldRequest::Copy(path.clone()));
                ui.close();
            }
            if action_enabled(ui, editable, &INSPECTOR_PASTE_VALUE, None).clicked() {
                picks.push(FieldRequest::Paste(path.clone()));
                ui.close();
            }
            if action_button_text(ui, &INSPECTOR_COPY_RON, "Copy as RON").clicked() {
                picks.push(FieldRequest::CopyRon(path.clone()));
                ui.close();
            }
            if action_button_text(ui, &INSPECTOR_COPY_AGENT_PATH, "Copy agent path").clicked() {
                picks.push(FieldRequest::CopyAgentPath(path.clone()));
                ui.close();
            }
            if action_enabled(ui, can_reset, &INSPECTOR_RESET_FIELD, None).clicked() {
                picks.push(FieldRequest::Reset(path.clone()));
                ui.close();
            }
            ui.separator();
            // Key the field into the open sequence (ADR-9710), or pin it to the top of its
            // component (inspector::pinned).
            if ui.button("Add to sequence").clicked() {
                self.context.sequence_requests.push(path.clone());
                ui.close();
            }
            let pin = if pinned { "Unpin from top" } else { "Pin to top" };
            if crate::agent_parity::action_button_text(ui, &crate::agent_parity::INSPECTOR_PIN_FIELD, pin)
                .clicked()
            {
                self.context.pin_requests.push((path.clone(), !pinned));
                ui.close();
            }
        });
        if response.has_focus() {
            let (copy, paste, shift) = response.ctx.input(|i| {
                (
                    i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                    i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))),
                    i.modifiers.shift,
                )
            });
            if copy {
                picks.push(if shift {
                    FieldRequest::CopyAgentPath(path.clone())
                } else {
                    FieldRequest::Copy(path.clone())
                });
            }
            if paste && editable {
                picks.push(FieldRequest::Paste(path.clone()));
            }
        }
        self.context.field_requests.extend(picks);
    }

    /// The tooltip of the field at `field_path`.
    fn field_tooltip(
        &self,
        field_path: &str,
        attrs: Option<&EditorAttrs>,
        doc: Option<&str>,
        blocked: Option<ConditionState>,
    ) -> super::tooltip::FieldTooltip {
        let doc = attrs
            .and_then(|a| a.tooltip)
            .map(str::to_owned)
            .or_else(|| doc.and_then(super::tooltip::first_paragraph));
        let (default, default_source) = match self.context.tools.as_ref() {
            Some(tools) => (
                tools.baseline_ron(self.type_registry, field_path),
                tools.baseline_source,
            ),
            None => (None, "default"),
        };
        super::tooltip::FieldTooltip {
            doc,
            default,
            default_source,
            agent_path: super::tooltip::agent_path(&self.context.component, field_path),
            condition: blocked.map(|c| c.source.to_owned()),
        }
    }

    fn ui_for_struct(
        &mut self,
        value: &mut dyn Struct,
        ui: &mut egui::Ui,
        id: egui::Id,
        options: &dyn Any,
    ) -> bool {
        let Some(TypeInfo::Struct(type_info)) = value.get_represented_type_info() else {
            return false;
        };
        let component = self.context.component.clone();
        let derived: Vec<FieldOptions> = type_info
            .iter()
            .map(|f| {
                FieldOptions::for_field(
                    &component,
                    f.name(),
                    EditorAttrs::from_custom(f.custom_attributes()),
                )
            })
            .collect();
        let mut changed = false;
        // Fields with @Category draw under sub-headers, uncategorized first (inspector::categories).
        let categories: Vec<Option<&'static str>> = derived.iter().map(|f| f.attrs.category).collect();
        let parts = StructParts {
            info: type_info,
            derived: &derived,
            options,
            id,
        };
        if !super::categories::has_categories(&categories) {
            let all: Vec<usize> = (0..value.field_len()).collect();
            Grid::new(id).num_columns(2).show(ui, |ui| {
                changed |= self.struct_rows(value, &parts, &all, ui);
            });
            return changed;
        }
        for (category, indices) in super::categories::group_fields(&categories) {
            match category {
                None => {
                    Grid::new(id).num_columns(2).show(ui, |ui| {
                        changed |= self.struct_rows(value, &parts, &indices, ui);
                    });
                }
                Some(name) => {
                    let visible = indices.as_slice().iter().any(|&i| {
                        type_info
                            .field_at(i)
                            .is_some_and(|f| self.context.row_visible(&self.context.child_field(f.name())))
                    });
                    if !visible {
                        continue;
                    }
                    let sub = id.with(("category", name));
                    let header = egui::CollapsingHeader::new(name).id_salt(sub).default_open(true);
                    let header = if self.context.filtering() {
                        header.open(Some(true))
                    } else {
                        header
                    };
                    header.show(ui, |ui| {
                        Grid::new(sub).num_columns(2).show(ui, |ui| {
                            changed |= self.struct_rows(value, &parts, &indices, ui);
                        });
                    });
                }
            }
        }
        changed
    }

    /// The rows of struct fields `indices` (one grid's worth of [`Self::field_row`]s).
    fn struct_rows(
        &mut self,
        value: &mut dyn Struct,
        parts: &StructParts<'_>,
        indices: &[usize],
        ui: &mut egui::Ui,
    ) -> bool {
        let mut changed = false;
        let id = parts.id;
        for &i in indices {
            let (Some(field_info), Some(fallback)) = (parts.info.field_at(i), parts.derived.get(i)) else {
                continue;
            };
            let field_options = struct_field_options(parts.options, i, fallback);
            let name = field_info.name();
            // @EditCondition reads the sibling fields; @Toggle the group's own `enabled`.
            let attrs = field_options
                .downcast_ref::<FieldOptions>()
                .map_or(&fallback.attrs, |f| &f.attrs);
            let condition = ConditionState::of(attrs, value.as_partial_reflect());
            let toggle = value.field_at(i).and_then(|field| toggle_state(attrs, field));
            let row = RowInfo {
                doc: field_info.docs(),
                condition,
                toggle,
            };
            let enabled_path = toggle.map(|_| format!("{}.enabled", self.context.child_field(name)));
            if let Some(path) = &enabled_path {
                self.context.toggle_paths.insert(path.clone());
            }
            let Some(field) = value.field_at_mut(i) else {
                continue;
            };
            let row_changed = self.field_row(ui, name, field_options, row, |this, ui| {
                this.with_segment(Segment::Field(name), |this| {
                    // A @Toggle group that is off draws greyed (P-121).
                    if toggle == Some(false) {
                        ui.add_enabled_ui(false, |ui| {
                            this.suppressed(|this| {
                                this.ui_for_reflect_with_options(field, ui, id.with(i), field_options)
                            })
                        });
                        false
                    } else {
                        this.ui_for_reflect_with_options(field, ui, id.with(i), field_options)
                    }
                })
            });
            if let Some(path) = &enabled_path {
                self.context.toggle_paths.remove(path);
            }
            if let Some(c) = row_changed {
                changed |= c;
            }
            // The header checkbox flips `enabled` as its own undo step.
            if let Some(on) = self.context.toggle_pick.take()
                && let Some(ReflectMut::Struct(group)) =
                    value.field_at_mut(i).map(|field| field.reflect_mut())
                && let Some(enabled) = group
                    .field_mut("enabled")
                    .and_then(|f| f.try_downcast_mut::<bool>())
            {
                *enabled = on;
                self.with_segment(Segment::Field(name), |this| {
                    this.with_segment(Segment::Field("enabled"), |this| this.context.record(true));
                });
                changed = true;
            }
        }
        changed
    }

    /// Options for the unnamed fields of a tuple struct/tuple: their attributes, or the parent's
    /// field options for a single-field newtype.
    fn unnamed_options(&self, attrs: Vec<EditorAttrs>, parent: &dyn Any) -> Vec<FieldOptions> {
        let parent = parent.downcast_ref::<FieldOptions>();
        let single = attrs.len() == 1;
        attrs
            .into_iter()
            .enumerate()
            .map(|(i, attrs)| match parent {
                Some(parent) if single && attrs.is_empty() => parent.clone(),
                _ => FieldOptions::for_field(&self.context.component, &i.to_string(), attrs),
            })
            .collect()
    }

    fn ui_for_tuple_struct(
        &mut self,
        value: &mut dyn TupleStruct,
        ui: &mut egui::Ui,
        id: egui::Id,
        options: &dyn Any,
    ) -> bool {
        let attrs: Vec<EditorAttrs> = match value.get_represented_type_info() {
            Some(TypeInfo::TupleStruct(info)) => info
                .iter()
                .map(|f| EditorAttrs::from_custom(f.custom_attributes()))
                .collect(),
            _ => vec![EditorAttrs::default(); value.field_len()],
        };
        let derived = self.unnamed_options(attrs, options);
        let len = value.field_len();
        maybe_grid(len, ui, id, |ui, label| {
            let mut changed = false;
            for i in 0..len {
                let Some(fallback) = derived.get(i) else { continue };
                let field_options = struct_field_options(options, i, fallback);
                let Some(field) = value.field_mut(i) else { continue };
                let index = i.to_string();
                if label {
                    if let Some(c) =
                        self.field_row(ui, &index, field_options, RowInfo::default(), |this, ui| {
                            this.with_segment(Segment::Field(&index), |this| {
                                this.ui_for_reflect_with_options(field, ui, id.with(i), field_options)
                            })
                        })
                    {
                        changed |= c;
                    }
                } else {
                    changed |= self.with_segment(Segment::Field(&index), |this| {
                        this.ui_for_reflect_with_options(field, ui, id.with(i), field_options)
                    });
                }
            }
            changed
        })
    }

    fn ui_for_tuple(
        &mut self,
        value: &mut dyn Tuple,
        ui: &mut egui::Ui,
        id: egui::Id,
        options: &dyn Any,
    ) -> bool {
        let len = value.field_len();
        let derived = self.unnamed_options(vec![EditorAttrs::default(); len], options);
        maybe_grid(len, ui, id, |ui, label| {
            let mut changed = false;
            for i in 0..len {
                let Some(fallback) = derived.get(i) else { continue };
                let field_options = struct_field_options(options, i, fallback);
                let Some(field) = value.field_mut(i) else { continue };
                let index = i.to_string();
                if label {
                    ui.label(&index);
                }
                changed |= self.with_segment(Segment::Field(&index), |this| {
                    this.ui_for_reflect_with_options(field, ui, id.with(i), field_options)
                });
                if label {
                    ui.end_row();
                }
            }
            changed
        })
    }

    fn ui_for_list(
        &mut self,
        list: &mut dyn List,
        ui: &mut egui::Ui,
        id: egui::Id,
        options: &dyn Any,
    ) -> bool {
        let mut changed = false;
        let error_id = id.with("error");
        ui.vertical(|ui| {
            let mut op = None;
            let len = list.len();
            // The header: element count and Clear (with a confirm step), P-122.
            if let Some(header_op) = list_header(ui, id, len) {
                op = Some(header_op);
            }
            if len == 0 && ui_for_empty_collection(ui, "(empty list)") {
                op = Some(ListOp::AddElement(0));
            }
            let page = pager(ui, id, len);
            let list_key = id.value();
            for i in page.clone() {
                let row = Grid::new((id, i)).show(ui, |ui| {
                    // The drag handle: drag onto another item to move there; Alt+Up/Down while
                    // it has focus moves one step.
                    if let Some(handle_op) = list_drag_handle(ui, list_key, i, len) {
                        op = Some(handle_op);
                    }
                    ui.label(i.to_string());
                    if let Some(element) = list.get_mut(i) {
                        ui.horizontal_top(|ui| {
                            changed |= self.with_segment(Segment::Index(i), |this| {
                                this.ui_for_reflect_with_options(element, ui, id.with(i), options)
                            });
                        });
                    }
                    ui.end_row();
                    ui.label("");
                    ui.label("");
                    if let Some(item_op) = ui_for_list_controls(ui, i, len) {
                        op = Some(item_op);
                    }
                });
                if let Some(drag) = row.response.dnd_release_payload::<ListDrag>()
                    && drag.list == list_key
                    && drag.index != i
                {
                    op = Some(ListOp::MoveElement {
                        from: drag.index,
                        to: i,
                    });
                }
                if i + 1 != page.end {
                    ui.separator();
                }
            }
            if let Some(op) = op {
                match apply_list_op(self.type_registry, list, op) {
                    Ok(()) => {
                        self.context.record(true);
                        changed = true;
                    }
                    Err(err) => {
                        tracing::debug!(target: "regia_editor", "list op: {err}");
                        ui.data_mut(|data| data.insert_temp::<bool>(error_id, true));
                    }
                }
            }
            if ui.data_mut(|data| *data.get_temp_mut_or_default::<bool>(error_id)) {
                let type_path = list
                    .get_represented_type_info()
                    .map(TypeInfo::type_path)
                    .unwrap_or("?");
                no_default_value(ui, type_path);
            }
            if ui.input(|input| input.pointer.any_down()) {
                ui.data_mut(|data| data.insert_temp::<bool>(error_id, false));
            }
        });
        changed
    }

    fn ui_for_array(
        &mut self,
        array: &mut dyn Array,
        ui: &mut egui::Ui,
        id: egui::Id,
        options: &dyn Any,
    ) -> bool {
        let mut changed = false;
        ui.vertical(|ui| {
            let len = array.len();
            let page = pager(ui, id, len);
            for i in page.clone() {
                let Some(element) = array.get_mut(i) else { continue };
                ui.horizontal_top(|ui| {
                    changed |= self.with_segment(Segment::Index(i), |this| {
                        this.ui_for_reflect_with_options(element, ui, id.with(i), options)
                    });
                });
                if i + 1 != page.end {
                    ui.separator();
                }
            }
        });
        changed
    }

    /// Draws a read-only copy of `value` (map keys, set elements).
    fn ui_readonly(&mut self, value: &dyn PartialReflect, ui: &mut egui::Ui, id: egui::Id) {
        let Some(mut copy) = clone_concrete(self.type_registry, value) else {
            ui.weak("?");
            return;
        };
        ui.add_enabled_ui(false, |ui| {
            self.suppressed(|this| this.ui_for_reflect_with_options(copy.as_mut(), ui, id, &()));
        });
    }

    fn ui_for_map(&mut self, map: &mut dyn Map, ui: &mut egui::Ui, id: egui::Id) -> bool {
        let mut changed = false;
        let mut removed = false;
        if map.is_empty() {
            ui.label("(empty map)");
        }
        let page = pager(ui, id, map.len());
        Grid::new(id).show(ui, |ui| {
            let mut i = 0usize;
            map.retain(&mut |key, value| {
                let row_id = id.with(i);
                i += 1;
                // Entries off the page are kept without being laid out.
                if !page.contains(&(i - 1)) {
                    return true;
                }
                self.ui_readonly(key, ui, row_id.with("key"));
                // Map values have no reflect path: edits record the whole map (below).
                changed |= self.suppressed(|this| this.ui_for_reflect_with_options(value, ui, row_id, &()));
                let delete = remove_button(ui, "Remove element").clicked();
                ui.end_row();
                removed |= delete;
                !delete
            });
            if self.map_add_element_ui(map, ui, id) {
                removed = true;
            }
        });
        if removed {
            self.context.record(true);
        }
        changed || removed
    }

    /// The "new element" row of a map: a draft key/value kept in egui memory until inserted.
    /// Returns whether an element was inserted.
    fn map_add_element_ui(&mut self, map: &mut dyn Map, ui: &mut egui::Ui, id: egui::Id) -> bool {
        let Some(info) = map.get_represented_map_info() else {
            return false;
        };
        let (Some(key_default), Some(value_default)) = (
            self.type_registry
                .get_type_data::<ReflectDefault>(info.key_ty().id()),
            self.type_registry
                .get_type_data::<ReflectDefault>(info.value_ty().id()),
        ) else {
            return false;
        };
        let draft_id = id.with("map_draft");
        let draft = ui.data_mut(|data| data.get_temp::<MapDraft>(draft_id));
        let mut inserted = false;
        ui.label("New element");
        match draft {
            None => {
                if add_button(ui, "Add element").clicked() {
                    let draft = MapDraft {
                        key: SendBox(key_default.default().into_partial_reflect()),
                        value: SendBox(value_default.default().into_partial_reflect()),
                    };
                    ui.data_mut(|data| data.insert_temp(draft_id, draft));
                }
                ui.end_row();
            }
            Some(mut draft) => {
                ui.end_row();
                let edited = self.suppressed(|this| {
                    let key =
                        this.ui_for_reflect_with_options(draft.key.0.as_mut(), ui, draft_id.with("k"), &());
                    let value =
                        this.ui_for_reflect_with_options(draft.value.0.as_mut(), ui, draft_id.with("v"), &());
                    key || value
                });
                if ui.button("Insert").clicked() {
                    map.insert_boxed(draft.key.0, draft.value.0);
                    ui.data_mut(|data| data.remove::<MapDraft>(draft_id));
                    inserted = true;
                } else if ui.button("Cancel").clicked() {
                    ui.data_mut(|data| data.remove::<MapDraft>(draft_id));
                } else if edited {
                    ui.data_mut(|data| data.insert_temp(draft_id, draft));
                }
                ui.end_row();
            }
        }
        inserted
    }

    fn ui_for_set(&mut self, set: &mut dyn Set, ui: &mut egui::Ui, id: egui::Id, options: &dyn Any) -> bool {
        let mut remove: Option<Box<dyn PartialReflect>> = None;
        let mut insert: Option<Box<dyn PartialReflect>> = None;
        ui.vertical(|ui| {
            let len = set.len();
            if len == 0 {
                ui.label("(empty set)");
            }
            let page = pager(ui, id, len);
            for (i, element) in set.iter().enumerate().skip(page.start).take(page.len()) {
                Grid::new((id, i)).show(ui, |ui| {
                    self.ui_readonly(element, ui, id.with(i));
                    if remove_button(ui, "Remove element").clicked() {
                        remove = clone_value(element);
                    }
                    ui.end_row();
                });
            }
            insert = self.set_add_element_ui(set, ui, id, options);
        });
        let mut changed = false;
        if let Some(element) = remove {
            changed |= set.remove(element.as_ref());
        }
        if let Some(element) = insert {
            changed |= set.insert_boxed(element);
        }
        if changed {
            self.context.record(true);
        }
        changed
    }

    /// The "new element" row of a set: a draft value kept in egui memory until inserted.
    fn set_add_element_ui(
        &mut self,
        set: &dyn Set,
        ui: &mut egui::Ui,
        id: egui::Id,
        options: &dyn Any,
    ) -> Option<Box<dyn PartialReflect>> {
        let Some(TypeInfo::Set(info)) = set.get_represented_type_info() else {
            return None;
        };
        let default = self
            .type_registry
            .get_type_data::<ReflectDefault>(info.value_ty().id())?;
        let draft_id = id.with("set_draft");
        let draft = ui.data_mut(|data| data.get_temp::<SetDraft>(draft_id));
        let mut inserted = None;
        ui.horizontal(|ui| {
            ui.label("New element");
            match draft {
                None => {
                    if add_button(ui, "Add element").clicked() {
                        let draft = SetDraft(SendBox(default.default().into_partial_reflect()));
                        ui.data_mut(|data| data.insert_temp(draft_id, draft));
                    }
                }
                Some(mut draft) => {
                    let edited = self.suppressed(|this| {
                        this.ui_for_reflect_with_options(draft.0.0.as_mut(), ui, draft_id, options)
                    });
                    if ui.button("Insert").clicked() {
                        ui.data_mut(|data| data.remove::<SetDraft>(draft_id));
                        inserted = Some(draft.0.0);
                    } else if ui.button("Cancel").clicked() {
                        ui.data_mut(|data| data.remove::<SetDraft>(draft_id));
                    } else if edited {
                        ui.data_mut(|data| data.insert_temp(draft_id, draft));
                    }
                }
            }
        });
        inserted
    }

    fn ui_for_enum(
        &mut self,
        value: &mut dyn Enum,
        ui: &mut egui::Ui,
        id: egui::Id,
        options: &dyn Any,
    ) -> bool {
        let Some(TypeInfo::Enum(type_info)) = value.get_represented_type_info() else {
            ui.label("Unrepresentable");
            return false;
        };
        let mut changed = false;
        let is_option = type_info.type_path().starts_with("core::option::Option<");
        ui.vertical(|ui| {
            // An Option draws an enable checkbox in its header instead of the variant combo
            // (P-121): on is `Some(default)`, off is `None`, one undo step per toggle.
            if is_option && !self.context.is_mixed() {
                let mut on = value.variant_name() == "Some";
                let spoken = format!("Enable {}", self.field_name());
                let response = crate::agent_parity::action_bare_checkbox(
                    ui,
                    &crate::agent_parity::INSPECTOR_TOGGLE_GROUP,
                    &mut on,
                    &spoken,
                );
                if response.changed() {
                    match switch_variant(self.type_registry, value, if on { "Some" } else { "None" }) {
                        Ok(()) => {
                            self.context.record(true);
                            changed = true;
                        }
                        Err(err) => tracing::debug!(target: "regia_editor", "option toggle: {err}"),
                    }
                }
            } else if let Some(dynamic) =
                self.ui_for_enum_variant_select(id, ui, value.variant_index(), type_info)
            {
                match value.try_apply(&dynamic) {
                    Ok(()) => {
                        self.context.record(true);
                        changed = true;
                    }
                    Err(err) => tracing::warn!(target: "regia_editor", "variant switch: {err}"),
                }
            }
            // Differing variants: the combo above is the whole edit. Nested fields belong to the
            // sample's variant only, so they are not drawn.
            if self.context.is_mixed() {
                return;
            }
            let variant_index = value.variant_index();
            let variant_info = type_info.variant_at(variant_index);
            // `Option<T>`'s field takes the options of the field holding the option.
            let parent = options.downcast_ref::<FieldOptions>().filter(|_| is_option);
            let component = self.context.component.clone();
            let derived: Vec<FieldOptions> = (0..value.field_len())
                .map(|i| {
                    let (name, attrs) = match variant_info {
                        Some(VariantInfo::Struct(info)) => (
                            info.field_at(i).map(|f| f.name().to_string()).unwrap_or_default(),
                            info.field_at(i)
                                .map(|f| EditorAttrs::from_custom(f.custom_attributes()))
                                .unwrap_or_default(),
                        ),
                        Some(VariantInfo::Tuple(info)) => (
                            i.to_string(),
                            info.field_at(i)
                                .map(|f| EditorAttrs::from_custom(f.custom_attributes()))
                                .unwrap_or_default(),
                        ),
                        _ => (i.to_string(), EditorAttrs::default()),
                    };
                    match parent {
                        Some(parent) if attrs.is_empty() => parent.clone(),
                        _ => FieldOptions::for_field(&component, &name, attrs),
                    }
                })
                .collect();
            let always_show_label = matches!(value.variant_type(), VariantType::Struct);
            changed |= maybe_grid_label_if(value.field_len(), ui, id, always_show_label, |ui, label| {
                let mut changed = false;
                for i in 0..value.field_len() {
                    let name = value
                        .name_at(i)
                        .map(str::to_string)
                        .unwrap_or_else(|| i.to_string());
                    let Some(fallback) = derived.get(i) else { continue };
                    let field_options = variant_field_options(options, variant_index, i, fallback);
                    let Some(field) = value.field_at_mut(i) else {
                        continue;
                    };
                    let mut draw = |this: &mut Self, ui: &mut egui::Ui| {
                        this.with_segment(Segment::Field(&name), |this| {
                            this.ui_for_reflect_with_options(field, ui, id.with(i), field_options)
                        })
                    };
                    if label {
                        if let Some(c) = self.field_row(ui, &name, field_options, RowInfo::default(), draw) {
                            changed |= c;
                        }
                    } else {
                        changed |= draw(self, ui);
                    }
                }
                changed
            });
        });
        changed
    }

    /// The variant combo box; returns the default-constructed new variant when one is picked.
    fn ui_for_enum_variant_select(
        &mut self,
        id: egui::Id,
        ui: &mut egui::Ui,
        active_variant_idx: usize,
        info: &EnumInfo,
    ) -> Option<DynamicEnum> {
        let mut changed_variant = None;
        let active = info
            .variant_names()
            .get(active_variant_idx)
            .copied()
            .unwrap_or("?");
        // The variant itself differs across the selection: the combo shows the mixed marker and
        // the caller skips the variant's fields (they are not one value).
        let selected = if self.context.is_mixed() { "—" } else { active };
        egui::ComboBox::new(id.with("select"), "")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for (i, variant) in info.iter().enumerate() {
                    let constructed = default_variant(self.type_registry, variant);
                    ui.add_enabled_ui(constructed.is_ok(), |ui| {
                        let mut response = ui.selectable_label(i == active_variant_idx, variant.name());
                        if let Err(fields) = &constructed {
                            response = response.on_disabled_hover_ui(|ui| {
                                unconstructable_variant(ui, info.type_path(), variant.name(), fields);
                            });
                        }
                        if response.clicked()
                            && i != active_variant_idx
                            && let Ok(dynamic) = constructed
                        {
                            changed_variant = Some(dynamic);
                        }
                    });
                }
            });
        changed_variant
    }
}

/// A boxed reflected value in egui's temp memory (which needs `Clone + Send + Sync`).
struct SendBox(Box<dyn PartialReflect>);

impl Clone for SendBox {
    fn clone(&self) -> Self {
        Self(clone_value(self.0.as_ref()).unwrap_or_else(|| Box::new(())))
    }
}

/// A map's draft element.
#[derive(Clone)]
struct MapDraft {
    key: SendBox,
    value: SendBox,
}

/// A set's draft element.
#[derive(Clone)]
struct SetDraft(SendBox);

/// Containers longer than this are paginated (ADR-7306): a 100 000-element list lays out one
/// page, never every element.
pub const PAGE_THRESHOLD: usize = 200;

/// Elements per page of a paginated container.
pub const PAGE_SIZE: usize = 100;

/// The element range of page `page` (clamped to the last page) of a container of `len` elements;
/// the whole container at or under [`PAGE_THRESHOLD`].
#[must_use]
pub fn page_range(len: usize, page: usize) -> std::ops::Range<usize> {
    if len <= PAGE_THRESHOLD {
        return 0..len;
    }
    let last = len.div_ceil(PAGE_SIZE) - 1;
    let start = page.min(last) * PAGE_SIZE;
    start..(start + PAGE_SIZE).min(len)
}

/// The page of a container `len` long to draw, with the pager row (first / previous / next /
/// last, and a jump to an index) above it when the container is paginated. The page lives in
/// egui memory under `id`.
fn pager(ui: &mut egui::Ui, id: egui::Id, len: usize) -> std::ops::Range<usize> {
    if len <= PAGE_THRESHOLD {
        return 0..len;
    }
    let page_id = id.with("inspector-page");
    let jump_id = id.with("inspector-jump");
    let last = len.div_ceil(PAGE_SIZE) - 1;
    let mut page = ui.data(|d| d.get_temp::<usize>(page_id)).unwrap_or(0).min(last);
    let mut jump = ui.data(|d| d.get_temp::<String>(jump_id)).unwrap_or_default();
    ui.horizontal(|ui| {
        if ui
            .add_enabled(page > 0, egui::Button::new("\u{23ee}").small())
            .on_hover_text("First page")
            .clicked()
        {
            page = 0;
        }
        if ui
            .add_enabled(page > 0, egui::Button::new("\u{25c0}").small())
            .on_hover_text("Previous page")
            .clicked()
        {
            page -= 1;
        }
        let range = page_range(len, page);
        ui.label(format!(
            "{}..={} of {len}",
            range.start,
            range.end.saturating_sub(1)
        ));
        if ui
            .add_enabled(page < last, egui::Button::new("\u{25b6}").small())
            .on_hover_text("Next page")
            .clicked()
        {
            page += 1;
        }
        if ui
            .add_enabled(page < last, egui::Button::new("\u{23ed}").small())
            .on_hover_text("Last page")
            .clicked()
        {
            page = last;
        }
        let field = ui.add(
            egui::TextEdit::singleline(&mut jump)
                .hint_text("index")
                .desired_width(56.0),
        );
        let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui
            .small_button("Go")
            .on_hover_text("Show the page holding this index")
            .clicked()
            || entered)
            && let Ok(index) = jump.trim().parse::<usize>()
        {
            page = index.min(len - 1) / PAGE_SIZE;
        }
    });
    ui.data_mut(|d| {
        d.insert_temp(page_id, page);
        d.insert_temp(jump_id, jump);
    });
    page_range(len, page)
}

fn ui_for_empty_collection(ui: &mut egui::Ui, label: &str) -> bool {
    let mut add = false;
    ui.horizontal(|ui| {
        ui.weak(label);
        if add_button(ui, "Add element").clicked() {
            add = true;
        }
    });
    add
}

fn ui_for_list_controls(ui: &mut egui::Ui, index: usize, len: usize) -> Option<ListOp> {
    let mut op = None;
    ui.horizontal_top(|ui| {
        if add_button(ui, "Add element").clicked() {
            op = Some(ListOp::AddElement(index));
        }
        if remove_button(ui, &format!("Remove element {index}")).clicked() {
            op = Some(ListOp::RemoveElement(index));
        }
        ui.add_enabled_ui(index > 0, |ui| {
            if up_button(ui, &format!("Move element {index} up")).clicked() {
                op = Some(ListOp::MoveElementUp(index));
            }
        });
        ui.add_enabled_ui(index + 1 < len, |ui| {
            if down_button(ui, &format!("Move element {index} down")).clicked() {
                op = Some(ListOp::MoveElementDown(index));
            }
        });
        let duplicate = crate::agent_parity::action_button_text(
            ui,
            &crate::agent_parity::INSPECTOR_LIST_DUPLICATE,
            egui::RichText::new("\u{2398}").small(),
        );
        if crate::a11y_audit::name(
            duplicate,
            egui::WidgetType::Button,
            &format!("Duplicate element {index}"),
        )
        .clicked()
        {
            op = Some(ListOp::DuplicateElement(index));
        }
    });
    op
}

/// The drag payload of a list element's handle: which list (its widget id) and which element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListDrag {
    /// The list's widget id value.
    list: u64,
    /// The dragged element.
    index: usize,
}

/// A list element's drag handle (P-122): a focusable grip. Drag it onto another element to move
/// there; with focus, Alt+Up / Alt+Down move the element one step.
fn list_drag_handle(ui: &mut egui::Ui, list: u64, index: usize, len: usize) -> Option<ListOp> {
    // The grip is a label, which the parity scan does not see: its action route is noted here
    // (`editor.list_move`, review sprint-w12-rev-g3 #8).
    crate::agent_parity::action_note(&crate::agent_parity::INSPECTOR_LIST_MOVE);
    let response = ui.add(egui::Label::new("\u{2630}").sense(egui::Sense::click_and_drag()));
    let response = crate::a11y_audit::name(
        response,
        egui::WidgetType::Button,
        &format!("Drag to reorder element {index} (Alt+Up or Alt+Down)"),
    );
    response.dnd_set_drag_payload(ListDrag { list, index });
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    if !response.has_focus() {
        return None;
    }
    let (up, down) = ui.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowUp),
            i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowDown),
        )
    });
    match (up, down) {
        (true, _) if index > 0 => Some(ListOp::MoveElement {
            from: index,
            to: index - 1,
        }),
        (_, true) if index + 1 < len => Some(ListOp::MoveElement {
            from: index,
            to: index + 1,
        }),
        _ => None,
    }
}

/// A list's header row: `N items` and Clear, which asks to confirm before it empties the list.
fn list_header(ui: &mut egui::Ui, id: egui::Id, len: usize) -> Option<ListOp> {
    let confirm_id = id.with("clear-confirm");
    let mut op = None;
    ui.horizontal(|ui| {
        ui.weak(if len == 1 {
            "1 item".to_owned()
        } else {
            format!("{len} items")
        });
        if len == 0 {
            return;
        }
        let confirming = ui.data(|d| d.get_temp::<bool>(confirm_id)).unwrap_or(false);
        if !confirming {
            if crate::agent_parity::action_button_text(
                ui,
                &crate::agent_parity::INSPECTOR_LIST_CLEAR,
                "Clear",
            )
            .clicked()
            {
                ui.data_mut(|d| d.insert_temp(confirm_id, true));
            }
            return;
        }
        ui.label(format!("Remove all {len}?"));
        if crate::agent_parity::action_button_text(
            ui,
            &crate::agent_parity::INSPECTOR_LIST_CLEAR,
            "Clear all",
        )
        .clicked()
        {
            op = Some(ListOp::Clear);
            ui.data_mut(|d| d.remove::<bool>(confirm_id));
        }
        if ui.button("Keep").clicked() {
            ui.data_mut(|d| d.remove::<bool>(confirm_id));
        }
    });
    op
}

fn maybe_grid(
    len: usize,
    ui: &mut egui::Ui,
    id: egui::Id,
    f: impl FnOnce(&mut egui::Ui, bool) -> bool,
) -> bool {
    match len {
        0 => false,
        1 => f(ui, false),
        _ => Grid::new(id).show(ui, |ui| f(ui, true)).inner,
    }
}

fn maybe_grid_label_if(
    len: usize,
    ui: &mut egui::Ui,
    id: egui::Id,
    always_show_label: bool,
    f: impl FnOnce(&mut egui::Ui, bool) -> bool,
) -> bool {
    match len {
        0 => false,
        1 if !always_show_label => f(ui, false),
        _ => Grid::new(id).show(ui, |ui| f(ui, true)).inner,
    }
}

#[cfg(test)]
mod colour_gate_tests {
    use super::*;

    #[test]
    fn raw_arrays_only_get_the_picker_on_colour_fields() {
        let attrs = regia_ecs::attrs::EditorAttrs::default;
        let direction = FieldOptions::for_field("DirectionalLight", "direction", attrs());
        let viewport = FieldOptions::for_field("Camera", "viewport", attrs());
        let colour = FieldOptions::for_field("DirectionalLight", "color", attrs());
        assert!(!is_colour_field(Some(&direction)));
        assert!(!is_colour_field(Some(&viewport)));
        assert!(!is_colour_field(None));
        assert!(is_colour_field(Some(&colour)));
    }
}
