// SPDX-License-Identifier: MPL-2.0
//! Editor selection: an ordered multi-select set with a primary (last clicked) entity, shared
//! by the outliner, viewport, inspector and the agent protocol (`specs/19` §§ 2–3).

use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;

/// How a click modifies the selection (`specs/19` § 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectMode {
    /// Replace the selection (plain click).
    Replace,
    /// Toggle membership (Ctrl-click).
    Toggle,
    /// Add without removing (Shift-click).
    Add,
}

/// The editor's selection. Lives in the edited world as a resource so panels and the agent
/// protocol share one truth.
#[derive(Resource, Debug, Clone, Default)]
pub struct Selection {
    entities: Vec<Entity>,
    primary: Option<Entity>,
}

impl Selection {
    /// The selected entities, oldest first.
    #[must_use]
    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }

    /// The primary (last selected) entity.
    #[must_use]
    pub fn primary(&self) -> Option<Entity> {
        self.primary.filter(|e| self.entities.contains(e))
    }

    /// Whether `entity` is selected.
    #[must_use]
    pub fn contains(&self, entity: Entity) -> bool {
        self.entities.contains(&entity)
    }

    /// Number of selected entities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether nothing is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Applies a selection event: click on `entity` with `mode`.
    pub fn select(&mut self, entity: Entity, mode: SelectMode) {
        match mode {
            SelectMode::Replace => {
                self.entities.clear();
                self.entities.push(entity);
                self.primary = Some(entity);
            }
            SelectMode::Add => {
                if !self.entities.contains(&entity) {
                    self.entities.push(entity);
                }
                self.primary = Some(entity);
            }
            SelectMode::Toggle => {
                if let Some(pos) = self.entities.iter().position(|e| *e == entity) {
                    self.entities.remove(pos);
                    if self.primary == Some(entity) {
                        self.primary = self.entities.last().copied();
                    }
                } else {
                    self.entities.push(entity);
                    self.primary = Some(entity);
                }
            }
        }
    }

    /// Selects everything (Edit → Select All); the primary becomes the first entity.
    /// Editor-internal entities ([`crate::visibility::EditorHidden`]) are never selectable and
    /// are skipped.
    pub fn select_all(&mut self, world: &World) {
        self.entities.clear();
        for entity_ref in world.iter_entities() {
            let entity = entity_ref.id();
            if crate::visibility::is_hidden(world, entity) {
                continue;
            }
            self.entities.push(entity);
        }
        self.primary = self.entities.first().copied();
    }

    /// Selects exactly `entities` (the filter-aware Select all of the Outliner, ADR-7757: only
    /// the rows its search matches); the primary becomes the first one.
    pub fn select_only(&mut self, entities: &[Entity]) {
        self.entities.clear();
        for entity in entities {
            if !self.entities.contains(entity) {
                self.entities.push(*entity);
            }
        }
        self.primary = self.entities.first().copied();
    }

    /// Selects every selectable entity `keep` accepts (Select all under a filter).
    pub fn select_all_matching(&mut self, world: &World, keep: impl Fn(Entity) -> bool) {
        self.select_all(world);
        self.entities.retain(|e| keep(*e));
        self.primary = self.entities.first().copied();
    }

    /// Clears the selection.
    pub fn clear(&mut self) {
        self.entities.clear();
        self.primary = None;
    }

    /// Drops selected entities that no longer exist (call after despawns).
    pub fn retain_alive(&mut self, world: &World) {
        self.entities.retain(|e| world.get_entity(*e).is_ok());
        if self.primary.is_some_and(|p| !self.entities.contains(&p)) {
            self.primary = self.entities.last().copied();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Selection history (P-033, ADR-7766): back and forward through the last selections.

/// How many selections [`SelectionHistory`] keeps.
pub const SELECTION_HISTORY_LEN: usize = 50;

/// One selection as the history keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionSnapshot {
    /// The selected entities, oldest first.
    pub entities: Vec<Entity>,
    /// The primary entity.
    pub primary: Option<Entity>,
}

impl Selection {
    /// The selection as a [`SelectionSnapshot`].
    #[must_use]
    pub fn snapshot(&self) -> SelectionSnapshot {
        SelectionSnapshot {
            entities: self.entities.clone(),
            primary: self.primary(),
        }
    }

    /// Makes the selection `snapshot` (no undo entry: the selection is view state).
    pub fn restore(&mut self, snapshot: &SelectionSnapshot) {
        self.entities.clone_from(&snapshot.entities);
        self.primary = snapshot
            .primary
            .filter(|p| self.entities.contains(p))
            .or_else(|| self.entities.last().copied());
    }
}

/// Back and forward through the last [`SELECTION_HISTORY_LEN`] selections (Alt+Left and
/// Alt+Right, `editor.selection_history`). [`track`] records the selection once per editor frame;
/// a new selection after going back drops the forward entries. An empty selection is not recorded.
#[derive(Resource, Debug, Clone, Default)]
pub struct SelectionHistory {
    entries: Vec<SelectionSnapshot>,
    cursor: usize,
    paused: bool,
    /// The editor frame whose Alt+Left / Alt+Right was applied last (egui may run a frame twice).
    last_key_frame: Option<u64>,
}

impl SelectionHistory {
    /// The recorded selections, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[SelectionSnapshot] {
        &self.entries
    }

    /// The index of the current entry in [`Self::entries`] (0 when empty).
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether [`Self::back`] has an entry to go to.
    #[must_use]
    pub fn can_back(&self) -> bool {
        self.cursor > 0
    }

    /// Whether [`Self::forward`] has an entry to go to.
    #[must_use]
    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.entries.len()
    }

    /// Stops (true) or resumes (false) recording, while a search previews entities.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Whether recording is stopped.
    #[must_use]
    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Records `snapshot` as the newest entry unless it is empty, recording is paused or it is the
    /// current entry. Drops the entries after the cursor and the oldest beyond the cap. Whether it
    /// was recorded.
    pub fn record(&mut self, snapshot: SelectionSnapshot) -> bool {
        if self.paused || snapshot.entities.is_empty() || self.entries.get(self.cursor) == Some(&snapshot) {
            return false;
        }
        if !self.entries.is_empty() {
            self.entries.truncate(self.cursor + 1);
        }
        self.entries.push(snapshot);
        if self.entries.len() > SELECTION_HISTORY_LEN {
            let extra = self.entries.len() - SELECTION_HISTORY_LEN;
            self.entries.drain(..extra);
        }
        self.cursor = self.entries.len() - 1;
        true
    }

    /// Moves the cursor one entry back and returns that entry.
    pub fn back(&mut self) -> Option<&SelectionSnapshot> {
        if !self.can_back() {
            return None;
        }
        self.cursor -= 1;
        self.entries.get(self.cursor)
    }

    /// Moves the cursor one entry forward and returns that entry.
    pub fn forward(&mut self) -> Option<&SelectionSnapshot> {
        if !self.can_forward() {
            return None;
        }
        self.cursor += 1;
        self.entries.get(self.cursor)
    }

    /// Moves the cursor to entry `index` and returns it.
    pub fn jump(&mut self, index: usize) -> Option<&SelectionSnapshot> {
        if index >= self.entries.len() {
            return None;
        }
        self.cursor = index;
        self.entries.get(index)
    }

    /// Drops despawned entities from every entry, and entries left empty (the cursor follows).
    fn prune(&mut self, world: &World) {
        let current = self.cursor;
        let mut kept = Vec::with_capacity(self.entries.len());
        let mut cursor = 0;
        for (i, mut entry) in std::mem::take(&mut self.entries).into_iter().enumerate() {
            entry.entities.retain(|e| world.get_entity(*e).is_ok());
            if entry.primary.is_some_and(|p| !entry.entities.contains(&p)) {
                entry.primary = entry.entities.last().copied();
            }
            if entry.entities.is_empty() || kept.last() == Some(&entry) {
                continue;
            }
            if i <= current {
                cursor = kept.len();
            }
            kept.push(entry);
        }
        self.entries = kept;
        self.cursor = cursor.min(self.entries.len().saturating_sub(1));
    }
}

/// Records the world's selection in its [`SelectionHistory`] (the shell calls it once per editor
/// frame; cheap: one comparison). Whether a new entry was recorded.
pub fn track(world: &mut World) -> bool {
    let Some(selection) = world.get_resource::<Selection>() else {
        return false;
    };
    // The common case, nothing changed, compares in place (no copy, no allocation).
    let unchanged = world.get_resource::<SelectionHistory>().is_some_and(|h| {
        h.paused
            || h.entries.get(h.cursor).is_some_and(|e| {
                e.entities.as_slice() == selection.entities() && e.primary == selection.primary()
            })
    });
    if unchanged || selection.is_empty() {
        return false;
    }
    let snapshot = selection.snapshot();
    world
        .get_resource_or_insert_with(SelectionHistory::default)
        .record(snapshot)
}

/// Which way [`go`] moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryStep {
    /// The previous selection (Alt+Left).
    Back,
    /// The next selection (Alt+Right).
    Forward,
    /// The entry at this index of [`SelectionHistory::entries`] (the history list).
    To(usize),
}

/// Records the current selection, then moves through the history and selects the entry it lands
/// on (despawned entities are dropped first). The entry selected, `None` when there is none that way.
pub fn go(world: &mut World, step: HistoryStep) -> Option<SelectionSnapshot> {
    track(world);
    let mut history = world.remove_resource::<SelectionHistory>().unwrap_or_default();
    history.prune(world);
    let landed = match step {
        HistoryStep::Back => history.back().cloned(),
        HistoryStep::Forward => history.forward().cloned(),
        HistoryStep::To(index) => history.jump(index).cloned(),
    };
    world.insert_resource(history);
    let snapshot = landed?;
    world
        .get_resource_or_insert_with(Selection::default)
        .restore(&snapshot);
    Some(snapshot)
}

/// The Alt+Left / Alt+Right keys of the selection history, isolated here so the scoped-shortcut
/// system can move them into the Outliner and Inspector scope (ADR-7766). A key a panel used this
/// frame (a focused graph row moves its node with Alt+arrow) is gone by the end of the frame, so
/// the keys are read at the end of the egui pass and applied at the start of the next frame;
/// nothing happens while a text field has the focus or during play. Returns the step applied.
pub fn history_keys(ctx: &egui::Context, world: &mut World, playing: bool) -> Option<HistoryStep> {
    let hook = egui::Id::new("regia-selection-history-keys");
    if !ctx.data(|d| d.get_temp::<bool>(hook).unwrap_or(false)) {
        ctx.data_mut(|d| d.insert_temp(hook, true));
        ctx.on_end_pass(
            "regia-selection-history-keys",
            std::sync::Arc::new(move |ui: &mut egui::Ui| {
                let ctx = ui.ctx().clone();
                if ctx.text_edit_focused() {
                    return;
                }
                let step = ctx.input(|i| {
                    i.events.iter().find_map(|e| match e {
                        egui::Event::Key {
                            key: key @ (egui::Key::ArrowLeft | egui::Key::ArrowRight),
                            pressed: true,
                            modifiers,
                            ..
                        } if modifiers.alt && !modifiers.command && !modifiers.shift => {
                            Some(*key == egui::Key::ArrowRight)
                        }
                        _ => None,
                    })
                });
                if let Some(forward) = step {
                    let frame = ctx.cumulative_frame_nr();
                    ctx.data_mut(|d| d.insert_temp(hook.with("pending"), (frame, forward)));
                }
            }),
        );
    }
    let pending = ctx.data_mut(|d| d.remove_temp::<(u64, bool)>(hook.with("pending")))?;
    if playing {
        return None;
    }
    let (frame, forward) = pending;
    let mut history = world.get_resource_or_insert_with(SelectionHistory::default);
    if history.last_key_frame.is_some_and(|last| last >= frame) {
        return None;
    }
    history.last_key_frame = Some(frame);
    let step = if forward {
        HistoryStep::Forward
    } else {
        HistoryStep::Back
    };
    go(world, step).map(|_| step)
}

/// The history list: one row per recorded selection, newest first, the current one marked; a
/// click (or Enter on a focused row) selects it. Back and Forward buttons above.
pub fn history_list(ui: &mut egui::Ui, world: &mut World) {
    let history = world
        .get_resource::<SelectionHistory>()
        .cloned()
        .unwrap_or_default();
    let mut step = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(history.can_back(), egui::Button::new("Back (Alt+Left)"))
            .clicked()
        {
            step = Some(HistoryStep::Back);
        }
        if ui
            .add_enabled(history.can_forward(), egui::Button::new("Forward (Alt+Right)"))
            .clicked()
        {
            step = Some(HistoryStep::Forward);
        }
    });
    if history.entries().is_empty() {
        ui.weak("No selection recorded yet");
    }
    for (index, entry) in history.entries().iter().enumerate().rev() {
        let label = snapshot_label(world, entry);
        if ui.selectable_label(index == history.cursor(), label).clicked() {
            step = Some(HistoryStep::To(index));
        }
    }
    if let Some(step) = step {
        go(world, step);
    }
}

/// A short label for a history entry: the primary entity's name, plus how many more.
#[must_use]
pub fn snapshot_label(world: &World, snapshot: &SelectionSnapshot) -> String {
    let Some(first) = snapshot.primary.or_else(|| snapshot.entities.last().copied()) else {
        return "(nothing)".to_owned();
    };
    let name = if world.get_entity(first).is_ok() {
        crate::outliner::display_name(world, first)
    } else {
        "(deleted)".to_owned()
    };
    match snapshot.entities.len() {
        0 | 1 => name,
        n => format!("{name} and {} more", n - 1),
    }
}

/// `editor.selection_history { action?, index? }`.
fn selection_history_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::{INVALID_PARAMS, RpcError};
    let bad = |code: &str, message: &str| {
        RpcError::new(
            INVALID_PARAMS,
            code,
            message,
            "action: \"back\" | \"forward\" | \"list\" | \"go\" (with index)",
        )
    };
    let action = match params.get("action") {
        None | Some(serde_json::Value::Null) => "list",
        Some(serde_json::Value::String(s)) => s.as_str(),
        Some(_) => return Err(bad("bad_action", "`action` must be a string")),
    };
    let moved = match action {
        "list" => {
            track(world);
            None
        }
        "back" => Some(go(world, HistoryStep::Back)),
        "forward" => Some(go(world, HistoryStep::Forward)),
        "go" => {
            let index = params
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| bad("bad_index", "`go` needs `index`, an entry index from `list`"))?;
            track(world);
            let len = world
                .get_resource::<SelectionHistory>()
                .map_or(0, |h| h.entries().len());
            if index as usize >= len {
                return Err(bad("bad_index", "`index` is past the last entry"));
            }
            Some(go(world, HistoryStep::To(index as usize)))
        }
        _ => return Err(bad("bad_action", "unknown `action`")),
    };
    let history = world
        .get_resource::<SelectionHistory>()
        .cloned()
        .unwrap_or_default();
    let entry_json = |snapshot: &SelectionSnapshot| {
        serde_json::json!({
            "entities": snapshot.entities.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
            "primary": snapshot.primary.map(Entity::to_bits),
            "label": snapshot_label(world, snapshot),
        })
    };
    Ok(serde_json::json!({
        "action": action,
        "moved": moved.map(|m| m.is_some()),
        "cursor": history.cursor(),
        "can_back": history.can_back(),
        "can_forward": history.can_forward(),
        "entries": history.entries().iter().map(entry_json).collect::<Vec<_>>(),
        "selection": world
            .get_resource::<Selection>()
            .map(|s| s.entities().iter().map(|e| e.to_bits()).collect::<Vec<_>>())
            .unwrap_or_default(),
    }))
}

/// The selection history's agent method, installed by `agent_ext::all_methods`.
#[must_use]
pub fn history_methods() -> Vec<(regia_agent::schema::MethodSchema, crate::viewport_agent::Handler)> {
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    vec![(
        MethodSchema {
            name: "editor.selection_history",
            tool: "selection_history",
            description: "Back and forward through the last 50 editor selections, as Alt+Left and Alt+Right do in \
                the Outliner and Inspector. action list (default) records the current selection and lists the \
                entries, oldest first; back and forward select the previous or next entry; go selects entry \
                index. A new selection after going back drops the forward entries.",
            params: &[
                ParamSchema {
                    name: "action",
                    kind: "string",
                    required: false,
                    description: "list (default), back, forward or go.",
                },
                ParamSchema {
                    name: "index",
                    kind: "integer",
                    required: false,
                    description: "The entry index for go (from list's entries).",
                },
            ],
            result: "{ action, moved: bool | null, cursor, can_back, can_forward, entries: [{ entities, primary, label }], selection }",
            mutating: true,
            permission: Permission::EditScene,
        },
        selection_history_method as crate::viewport_agent::Handler,
    )]
}

// ---------------------------------------------------------------------------------------------
// Region selection, select similar and the selection root (P-064, P-065, P-074; ADR-7769)

/// How a box or lasso changes the selection (`specs/19` § 3, ADR-7769): the held modifiers at
/// the release. Ctrl+Shift subtracts, Ctrl toggles, Shift adds, none replaces. A click uses the
/// same modes with a one-entity region.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RegionMode {
    /// The region replaces the selection.
    #[default]
    Replace,
    /// The region is added.
    Add,
    /// The region is removed.
    Subtract,
    /// Each entity in the region flips membership.
    Toggle,
}

impl RegionMode {
    /// The mode for the held modifiers (`ctrl`, `shift`): Ctrl+Shift subtracts, Ctrl toggles,
    /// Shift adds, none replaces.
    #[must_use]
    pub fn from_modifiers(ctrl: bool, shift: bool) -> Self {
        match (ctrl, shift) {
            (true, true) => Self::Subtract,
            (true, false) => Self::Toggle,
            (false, true) => Self::Add,
            (false, false) => Self::Replace,
        }
    }

    /// The agent name: `set`, `add`, `subtract` or `toggle`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Replace => "set",
            Self::Add => "add",
            Self::Subtract => "subtract",
            Self::Toggle => "toggle",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Replace, Self::Add, Self::Subtract, Self::Toggle]
            .into_iter()
            .find(|mode| mode.name() == name)
    }
}

impl Selection {
    /// Removes `entity` from the selection (nothing when it is not selected); the primary falls
    /// back to the newest remaining entity.
    pub fn deselect(&mut self, entity: Entity) {
        if let Some(pos) = self.entities.iter().position(|e| *e == entity) {
            self.entities.remove(pos);
            if self.primary == Some(entity) {
                self.primary = self.entities.last().copied();
            }
        }
    }

    /// Applies a box, lasso or click region `found` with `mode` (see [`RegionMode`]).
    pub fn apply_region(&mut self, found: &[Entity], mode: RegionMode) {
        if mode == RegionMode::Replace {
            self.clear();
        }
        for entity in found {
            match mode {
                RegionMode::Replace | RegionMode::Add => self.select(*entity, SelectMode::Add),
                RegionMode::Toggle => self.select(*entity, SelectMode::Toggle),
                RegionMode::Subtract => self.deselect(*entity),
            }
        }
    }
}

/// The pixels a lasso covers, row by row: for each pixel row whose center lies inside the
/// polygon's vertical extent, the half-open column spans `[x0, x1)` whose pixel centers are
/// inside `polygon` under the even-odd rule (a self-crossing loop leaves its doubly wound parts
/// out). `polygon` is in pixels, implicitly closed; fewer than three points cover nothing.
/// Rows and columns are clipped to `width` x `height`.
#[must_use]
pub fn lasso_spans(polygon: &[[f32; 2]], width: u32, height: u32) -> Vec<(u32, u32, u32)> {
    let mut spans = Vec::new();
    if polygon.len() < 3 || width == 0 || height == 0 || polygon.iter().flatten().any(|v| !v.is_finite()) {
        return spans;
    }
    let (mut y_min, mut y_max) = (f32::INFINITY, f32::NEG_INFINITY);
    for p in polygon {
        y_min = y_min.min(p[1]);
        y_max = y_max.max(p[1]);
    }
    let row0 = (y_min - 0.5).ceil().max(0.0) as i64;
    let row1 = ((y_max - 0.5).floor() as i64).min(i64::from(height) - 1);
    if row1 < row0 {
        return spans;
    }
    let mut crossings: Vec<f32> = Vec::new();
    for row in row0 as u32..=row1 as u32 {
        let yc = row as f32 + 0.5;
        crossings.clear();
        for (i, a) in polygon.iter().enumerate() {
            let b = polygon[(i + 1) % polygon.len()];
            // Half-open in y so a vertex on the scanline counts once.
            if (a[1] <= yc) != (b[1] <= yc) {
                let t = (yc - a[1]) / (b[1] - a[1]);
                crossings.push(a[0] + t * (b[0] - a[0]));
            }
        }
        crossings.sort_by(f32::total_cmp);
        for pair in crossings.chunks(2) {
            let [xa, xb] = [pair[0], pair.get(1).copied().unwrap_or(pair[0])];
            // Columns whose center x + 0.5 lies in [xa, xb).
            let c0 = (xa - 0.5).ceil().max(0.0) as u32;
            let c1 = ((xb - 0.5).ceil().max(0.0) as u32).min(width);
            if c0 < c1 {
                spans.push((row, c0, c1));
            }
        }
    }
    spans
}

/// Every entity with at least one visible id-buffer pixel inside the lasso `polygon` (pixels,
/// pixel centers tested, even-odd rule; [`lasso_spans`]), sorted by entity bits: the box
/// select's rule with a freehand outline.
#[must_use]
pub fn entities_in_lasso(ids: &crate::picking::IdBuffer, polygon: &[[f32; 2]]) -> Vec<Entity> {
    let _span = tracing::info_span!(target: "regia_editor", "lasso_select").entered();
    let mut found = std::collections::BTreeSet::new();
    for (row, x0, x1) in lasso_spans(polygon, ids.width, ids.height) {
        let start = (row * ids.width) as usize;
        for id in ids
            .ids
            .get(start + x0 as usize..start + x1 as usize)
            .unwrap_or_default()
        {
            if *id != 0 {
                found.insert(*id);
            }
        }
    }
    let mut out: Vec<Entity> = found
        .into_iter()
        .filter_map(|id| ids.entities.get(id as usize - 1).copied())
        .collect();
    out.sort_by_key(|e| e.to_bits());
    out.dedup();
    out
}

/// What **Select similar** compares (Edit ▸ Select, the Outliner's context menu,
/// `editor.select_similar { by }`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimilarBy {
    /// The declared kind; without one, the derived role, with every light type one kind.
    Kind,
    /// The same set of component types (editor markers ignored).
    Components,
    /// A shared mesh, material, sound or prefab source.
    Asset,
    /// A shared layer (entities in no layer match each other).
    Layer,
}

impl SimilarBy {
    /// Every criterion, in menu order.
    pub const ALL: [Self; 4] = [Self::Kind, Self::Components, Self::Asset, Self::Layer];

    /// The agent name: `kind`, `components`, `asset` or `layer`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::Components => "components",
            Self::Asset => "asset",
            Self::Layer => "layer",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|by| by.name() == name)
    }

    /// The palette command id (`selection.similar_<name>`).
    #[must_use]
    pub fn command(self) -> &'static str {
        match self {
            Self::Kind => "selection.similar_kind",
            Self::Components => "selection.similar_components",
            Self::Asset => "selection.similar_asset",
            Self::Layer => "selection.similar_layer",
        }
    }

    /// The menu label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Kind => "All of the same kind",
            Self::Components => "All with the same components",
            Self::Asset => "All using the same asset",
            Self::Layer => "All in the same layer",
        }
    }
}

/// The comparison key of one entity for `by` (`None`: it has nothing to compare, so it matches
/// only itself). Several strings: any shared one matches.
fn similar_keys(world: &World, entity: Entity, by: SimilarBy) -> Option<Vec<String>> {
    match by {
        SimilarBy::Kind => {
            let icon = crate::kind_icons::row_icon(world, entity);
            if let Some(kind) = icon.kind {
                return Some(vec![format!("kind:{kind}")]);
            }
            use regia_scene::kind::roles::Role;
            // A greybox shape or a resolved model draws a mesh through its part entities.
            let role = icon.role.or_else(|| {
                (world.get::<regia_render::Primitive>(entity).is_some()
                    || crate::mesh_source::has_resolved_parts(world, entity))
                .then_some(Role::Mesh)
            });
            role.map(|role| {
                vec![match role {
                    Role::Sun | Role::Spot | Role::Point => "role:light".to_owned(),
                    other => format!("role:{other:?}"),
                }]
            })
        }
        SimilarBy::Components => {
            let mut names: Vec<String> = world
                .inspect_entity(entity)
                .ok()?
                .map(|(_, info)| info.name().to_string())
                .filter(|name| !name.starts_with("regia_editor::"))
                .collect();
            names.sort();
            Some(vec![names.join("|")])
        }
        SimilarBy::Asset => {
            let mut keys = Vec::new();
            if let Some(source) = world.get::<regia_render::MeshSource>(entity) {
                for path in [&source.mesh, &source.material] {
                    if !path.is_empty() {
                        keys.push(format!("path:{path}"));
                    }
                }
            }
            if let Some(emitter) = world.get::<regia_audio::AudioEmitter>(entity)
                && !emitter.sound_name.is_empty()
            {
                keys.push(format!("path:{}", emitter.sound_name));
            }
            if let Some(instance) = world.get::<regia_scene::prefab::PrefabInstance>(entity) {
                keys.push(format!("prefab:{}", instance.prefab));
            }
            (!keys.is_empty()).then_some(keys)
        }
        SimilarBy::Layer => Some(match world.get::<regia_scene::layers::LayerMembership>(entity) {
            Some(membership) if !membership.layers.is_empty() => {
                membership.layers.iter().map(|l| format!("layer:{l}")).collect()
            }
            _ => vec!["layer:".to_owned()],
        }),
    }
}

/// Whether `entity` is level content a select-similar may take: not a resource, an
/// editor-internal entity, a model's part entity or locked.
fn similar_candidate(world: &World, entity: Entity) -> bool {
    world
        .get_entity(entity)
        .is_ok_and(|e| !e.contains::<regia_ecs::ecs::resource::IsResource>())
        && !crate::visibility::is_hidden(world, entity)
        && crate::mesh_source::pick_owner(world, entity) == entity
        && !crate::viewport_ops::is_locked(world, entity)
}

/// Every level entity similar to one of `seeds` by `by`, the seeds included, sorted by entity
/// bits. A seed with nothing to compare (no kind, no asset) contributes only itself.
#[must_use]
pub fn similar_entities(world: &World, seeds: &[Entity], by: SimilarBy) -> Vec<Entity> {
    let _span = tracing::info_span!(target: "regia_editor", "select_similar").entered();
    let mut wanted = std::collections::BTreeSet::new();
    let mut out: Vec<Entity> = Vec::new();
    for seed in seeds {
        if world.get_entity(*seed).is_err() {
            continue;
        }
        out.push(*seed);
        if let Some(keys) = similar_keys(world, *seed, by) {
            wanted.extend(keys);
        }
    }
    if !wanted.is_empty() {
        for entity in world.iter_entities().map(|e| e.id()) {
            if out.contains(&entity) || !similar_candidate(world, entity) {
                continue;
            }
            if similar_keys(world, entity, by).is_some_and(|keys| keys.iter().any(|k| wanted.contains(k))) {
                out.push(entity);
            }
        }
    }
    out.sort_by_key(|e| e.to_bits());
    out.dedup();
    out
}

/// **Select similar**: replaces the selection with [`similar_entities`] of the current
/// selection (the primary stays primary). Returns the selected entities; `None` when nothing is
/// selected.
pub fn select_similar(world: &mut World, by: SimilarBy) -> Option<Vec<Entity>> {
    let current = world.get_resource::<Selection>().cloned().unwrap_or_default();
    if current.is_empty() {
        return None;
    }
    let found = similar_entities(world, current.entities(), by);
    let mut next = Selection::default();
    next.apply_region(&found, RegionMode::Replace);
    if let Some(primary) = current.primary() {
        next.select(primary, SelectMode::Add);
    }
    world.insert_resource(next);
    Some(found)
}

/// The Edit ▸ Select items, one per [`SimilarBy`].
pub const EDIT_SIMILAR: [(&crate::agent_parity::EditorAction, SimilarBy); 4] = [
    (&crate::agent_parity::EDIT_SIMILAR_KIND, SimilarBy::Kind),
    (
        &crate::agent_parity::EDIT_SIMILAR_COMPONENTS,
        SimilarBy::Components,
    ),
    (&crate::agent_parity::EDIT_SIMILAR_ASSET, SimilarBy::Asset),
    (&crate::agent_parity::EDIT_SIMILAR_LAYER, SimilarBy::Layer),
];

/// The Outliner row menu's Select similar items.
pub const OUTLINER_SIMILAR: [(&crate::agent_parity::EditorAction, SimilarBy); 4] = [
    (&crate::agent_parity::OUTLINER_SIMILAR_KIND, SimilarBy::Kind),
    (
        &crate::agent_parity::OUTLINER_SIMILAR_COMPONENTS,
        SimilarBy::Components,
    ),
    (&crate::agent_parity::OUTLINER_SIMILAR_ASSET, SimilarBy::Asset),
    (&crate::agent_parity::OUTLINER_SIMILAR_LAYER, SimilarBy::Layer),
];

/// Draws the Edit ▸ Select items ([`EDIT_SIMILAR`]), enabled when something is selected.
/// Returns a status line when one ran.
pub fn similar_menu_items(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let any = world.get_resource::<Selection>().is_some_and(|s| !s.is_empty());
    let mut status = None;
    for (action, by) in EDIT_SIMILAR {
        if crate::agent_parity::action_enabled(ui, any, action, None).clicked() {
            status = crate::command_palette::run(world, by.command());
            ui.close();
        }
    }
    status
}

/// The viewport's selection shape for a primary drag (a world resource; absent means Box). The
/// `selection.toggle_lasso` command flips it; holding the lasso key (`selection.lasso`, L) at
/// the drag start draws a lasso either way.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SelectionShape {
    /// Drags draw a lasso instead of a box.
    pub lasso: bool,
}

impl SelectionShape {
    /// Whether drags in `world` draw a lasso.
    #[must_use]
    pub fn lasso_on(world: &World) -> bool {
        world.get_resource::<Self>().is_some_and(|shape| shape.lasso)
    }
}

/// A selection root (P-074, ADR-7769): a viewport click on any of this entity's descendants
/// selects this entity instead (the nearest marked ancestor wins); Ctrl+click still picks the
/// child. Saved with the level (an editor-meaning component; the game ignores it). A new
/// component type in the level's component map, not a change to the level format's shape.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct EditorSelectionRoot;

/// Registers [`EditorSelectionRoot`] with the world's type registry (when it has one) so a
/// level saves and loads it.
pub fn register_selection_types(world: &World) {
    if let Some(registry) = world.get_resource::<regia_ecs::reflect::AppTypeRegistry>() {
        registry.write().register::<EditorSelectionRoot>();
    }
}

/// What a viewport click on `entity` selects: the nearest of `entity` and its ancestors that
/// carries [`EditorSelectionRoot`], else `entity`. `child_pick` (Ctrl held) keeps `entity`.
#[must_use]
pub fn selection_root_of(world: &World, entity: Entity, child_pick: bool) -> Entity {
    if child_pick {
        return entity;
    }
    std::iter::once(entity)
        .chain(crate::hierarchy_ops::ancestors(world, entity))
        .find(|e| world.get::<EditorSelectionRoot>(*e).is_some())
        .unwrap_or(entity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_replace_add_and_toggle() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let c = world.spawn_empty().id();

        let mut selection = Selection::default();
        assert!(selection.is_empty());

        selection.select(a, SelectMode::Replace);
        selection.select(b, SelectMode::Add);
        assert_eq!(selection.entities(), &[a, b]);
        assert_eq!(selection.primary(), Some(b));

        selection.select(b, SelectMode::Toggle);
        assert_eq!(selection.entities(), &[a], "toggle removes");
        assert_eq!(selection.primary(), Some(a), "primary falls back");

        selection.select(c, SelectMode::Replace);
        assert_eq!(selection.entities(), &[c]);
    }

    #[test]
    fn select_all_and_retain_alive() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();

        let mut selection = Selection::default();
        selection.select(a, SelectMode::Replace);
        selection.select(b, SelectMode::Add);
        assert_eq!(selection.entities(), &[a, b]);
        assert_eq!(selection.primary(), Some(b));

        // Despawns prune from the selection.
        world.get_entity_mut(b).expect("alive").despawn();
        selection.retain_alive(&world);
        assert_eq!(selection.entities(), &[a]);

        // Select-all takes every alive entity except editor-internal ones.
        let hidden = world.spawn(crate::visibility::EditorHidden).id();
        let total = world.iter_entities().count();
        selection.select_all(&world);
        assert_eq!(selection.len(), total - 1);
        assert!(
            !selection.entities().contains(&hidden),
            "hidden entities are skipped"
        );
        assert_eq!(selection.primary(), Some(a));

        selection.clear();
        assert!(selection.is_empty());
    }

    #[test]
    fn select_only_and_select_all_matching_keep_the_filter() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let c = world.spawn_empty().id();
        let mut selection = Selection::default();
        selection.select(c, SelectMode::Replace);
        selection.select_only(&[b, a, b]);
        assert_eq!(selection.entities(), &[b, a], "duplicates collapse, order kept");
        assert_eq!(selection.primary(), Some(b));
        selection.select_all_matching(&world, |e| e != b);
        assert!(selection.contains(a) && selection.contains(c) && !selection.contains(b));
        selection.select_only(&[]);
        assert!(selection.is_empty() && selection.primary().is_none());
    }
}

#[cfg(test)]
mod history_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn select(world: &mut World, e: Entity) {
        world
            .get_resource_or_insert_with(Selection::default)
            .select(e, SelectMode::Replace);
        track(world);
    }

    fn selected(world: &World) -> Vec<Entity> {
        world.resource::<Selection>().entities().to_vec()
    }

    #[test]
    fn back_twice_then_a_new_selection_truncates_forward() {
        let mut world = World::new();
        let [a, b, c, d] = [(); 4].map(|()| world.spawn_empty().id());
        select(&mut world, a);
        select(&mut world, b);
        select(&mut world, c);
        assert_eq!(go(&mut world, HistoryStep::Back).unwrap().entities, [b]);
        assert_eq!(go(&mut world, HistoryStep::Back).unwrap().entities, [a]);
        assert_eq!(selected(&world), [a], "back twice selects A");
        assert!(go(&mut world, HistoryStep::Back).is_none(), "nothing before A");
        assert_eq!(go(&mut world, HistoryStep::Forward).unwrap().entities, [b]);
        select(&mut world, d);
        let history = world.resource::<SelectionHistory>();
        assert!(
            !history.can_forward(),
            "a new selection drops the forward entries"
        );
        let entries: Vec<Vec<Entity>> = history.entries().iter().map(|s| s.entities.clone()).collect();
        assert_eq!(entries, [vec![a], vec![b], vec![d]]);
    }

    #[test]
    fn keeps_the_last_fifty_and_skips_repeats_empties_and_pauses() {
        let mut world = World::new();
        let ids: Vec<Entity> = (0..60).map(|_| world.spawn_empty().id()).collect();
        for e in &ids {
            select(&mut world, *e);
            select(&mut world, *e);
        }
        let history = world.resource::<SelectionHistory>().clone();
        assert_eq!(history.entries().len(), SELECTION_HISTORY_LEN);
        assert_eq!(history.entries()[0].entities, [ids[10]]);
        world.resource_mut::<Selection>().clear();
        assert!(!track(&mut world), "an empty selection is not recorded");
        world.resource_mut::<SelectionHistory>().set_paused(true);
        select(&mut world, ids[0]);
        assert_eq!(
            world.resource::<SelectionHistory>().entries().len(),
            SELECTION_HISTORY_LEN
        );
    }

    #[test]
    fn despawned_entities_are_skipped() {
        let mut world = World::new();
        let [a, b, c] = [(); 3].map(|()| world.spawn_empty().id());
        select(&mut world, a);
        select(&mut world, b);
        select(&mut world, c);
        world.despawn(b);
        assert_eq!(go(&mut world, HistoryStep::Back).unwrap().entities, [a]);
    }

    #[test]
    fn agent_method_lists_and_walks() {
        let mut world = World::new();
        let [a, b] = [(); 2].map(|()| world.spawn_empty().id());
        select(&mut world, a);
        world.resource_mut::<Selection>().select(b, SelectMode::Replace);
        let (schema, handler) = &history_methods()[0];
        assert_eq!(schema.name, "editor.selection_history");
        let out = handler(&mut world, &serde_json::json!({})).unwrap();
        assert_eq!(
            out["entries"].as_array().unwrap().len(),
            2,
            "list records the selection"
        );
        let out = handler(&mut world, &serde_json::json!({ "action": "back" })).unwrap();
        assert_eq!(out["moved"], true);
        assert_eq!(out["selection"][0], a.to_bits());
        let out = handler(&mut world, &serde_json::json!({ "action": "go", "index": 1 })).unwrap();
        assert_eq!(out["selection"][0], b.to_bits());
        let code = |e: regia_agent::protocol::RpcError| e.data.unwrap()["code"].as_str().unwrap().to_owned();
        assert_eq!(
            code(handler(&mut world, &serde_json::json!({ "action": "sideways" })).unwrap_err()),
            "bad_action"
        );
        assert_eq!(
            code(handler(&mut world, &serde_json::json!({ "action": "go", "index": 9 })).unwrap_err()),
            "bad_index"
        );
    }

    /// The keyboard path: Alt+Left in a frame selects the previous entry at the next frame; a text
    /// field with the focus keeps the keys.
    #[test]
    fn alt_left_and_right_walk_the_history() {
        use crate::canvas_keys::test_input::{key, raw};
        let ctx = egui::Context::default();
        let mut world = World::new();
        let [a, b] = [(); 2].map(|()| world.spawn_empty().id());
        select(&mut world, a);
        select(&mut world, b);
        let run = |world: &mut World, events: Vec<egui::Event>| {
            let mut step = None;
            let mut out = ctx.run_ui(raw([400.0, 300.0], events), |ui| {
                step = history_keys(ui.ctx(), world, false);
            });
            out.textures_delta.clear();
            step
        };
        assert_eq!(
            run(&mut world, vec![key(egui::Key::ArrowLeft, egui::Modifiers::ALT)]),
            None
        );
        assert_eq!(
            run(&mut world, vec![]),
            Some(HistoryStep::Back),
            "applied at the next frame"
        );
        assert_eq!(selected(&world), [a]);
        assert_eq!(run(&mut world, vec![]), None, "applied once");
        run(&mut world, vec![key(egui::Key::ArrowRight, egui::Modifiers::ALT)]);
        assert_eq!(run(&mut world, vec![]), Some(HistoryStep::Forward));
        assert_eq!(selected(&world), [b]);
        // A plain arrow is not a history key.
        run(&mut world, vec![key(egui::Key::ArrowLeft, egui::Modifiers::NONE)]);
        assert_eq!(run(&mut world, vec![]), None);
    }

    /// `entities` in the order the region and similar functions return (entity bits).
    fn by_bits(mut entities: Vec<Entity>) -> Vec<Entity> {
        entities.sort_by_key(|e| e.to_bits());
        entities
    }

    /// A 12 x 8 id buffer with three 2 x 2 blocks: `a` at (1, 1), `b` at (5, 1), `c` at (9, 5).
    fn three_blocks() -> (crate::picking::IdBuffer, [Entity; 3]) {
        let mut world = World::new();
        let ents = [
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
        ];
        let (w, h) = (12u32, 8u32);
        let mut ids = vec![0u32; (w * h) as usize];
        for (n, (x0, y0)) in [(1u32, 1u32), (5, 1), (9, 5)].into_iter().enumerate() {
            for y in y0..y0 + 2 {
                for x in x0..x0 + 2 {
                    ids[(y * w + x) as usize] = n as u32 + 1;
                }
            }
        }
        let buffer = crate::picking::IdBuffer {
            width: w,
            height: h,
            distance: vec![1.0; ids.len()],
            ids,
            entities: ents.to_vec(),
        };
        (buffer, ents)
    }

    /// P-064 acceptance: a lasso drawn around two of three objects selects exactly those, and a
    /// Ctrl+Shift lasso around one of them removes it. The third object sits inside the lasso's
    /// bounding box (a box select would take it) but outside the polygon.
    #[test]
    fn a_lasso_over_two_of_three_selects_exactly_those_and_subtract_removes_one() {
        let (ids, [a, b, c]) = three_blocks();
        // A triangle over a and b whose bounding box also covers c's block.
        let lasso = [[0.0, 0.0], [8.0, 0.0], [0.0, 8.0]];
        let found = entities_in_lasso(&ids, &lasso);
        assert_eq!(found, by_bits(vec![a, b]), "the lasso takes a and b only");
        assert!(
            ids.entities_in_rect((0, 0), (11, 7)).contains(&c),
            "control: the lasso's bounding box covers c"
        );
        let mut selection = Selection::default();
        selection.apply_region(&[a, b], RegionMode::from_modifiers(false, false));
        assert_eq!(selection.entities(), &[a, b]);
        // Ctrl+Shift: a small lasso around b subtracts it.
        let around_b = [[4.5, 0.5], [7.5, 0.5], [7.5, 3.5], [4.5, 3.5]];
        let mode = RegionMode::from_modifiers(true, true);
        assert_eq!(mode, RegionMode::Subtract);
        selection.apply_region(&entities_in_lasso(&ids, &around_b), mode);
        assert_eq!(selection.entities(), &[a], "Ctrl+Shift removed b");
        // Shift adds c; Ctrl toggles a off.
        selection.apply_region(&[c], RegionMode::from_modifiers(false, true));
        selection.apply_region(&[a], RegionMode::from_modifiers(true, false));
        assert_eq!(selection.entities(), &[c]);
        assert_eq!(selection.primary(), Some(c));
    }

    /// The even-odd scanline fill: a concave "U" leaves its notch out, a self-crossing bow tie
    /// covers both lobes, degenerate input covers nothing.
    #[test]
    fn lasso_spans_follow_the_even_odd_rule() {
        // A U: outer 0..6 x 0..6 with a notch 2..4 x 0..4 cut from the top.
        let u = [
            [0.0, 0.0],
            [2.0, 0.0],
            [2.0, 4.0],
            [4.0, 4.0],
            [4.0, 0.0],
            [6.0, 0.0],
            [6.0, 6.0],
            [0.0, 6.0],
        ];
        let spans = lasso_spans(&u, 8, 8);
        assert!(
            spans.contains(&(0, 0, 2)) && spans.contains(&(0, 4, 6)),
            "{spans:?}"
        );
        assert!(
            spans.contains(&(5, 0, 6)),
            "below the notch the row is full: {spans:?}"
        );
        let covered: u32 = spans.iter().map(|(_, a, b)| b - a).sum();
        assert_eq!(covered, 36 - 8, "the notch's 8 pixels are out");
        assert!(lasso_spans(&[[0.0, 0.0], [5.0, 5.0]], 8, 8).is_empty());
        assert!(lasso_spans(&[[0.0, 0.0], [5.0, f32::NAN], [1.0, 3.0]], 8, 8).is_empty());
        // Clipped to the buffer.
        let big = [[-10.0, -10.0], [100.0, -10.0], [100.0, 100.0], [-10.0, 100.0]];
        let covered: u32 = lasso_spans(&big, 4, 3).iter().map(|(_, a, b)| b - a).sum();
        assert_eq!(covered, 12);
        assert_eq!(RegionMode::parse("subtract"), Some(RegionMode::Subtract));
        assert_eq!(RegionMode::parse("bogus"), None);
    }

    fn light(world: &mut World) -> Entity {
        world.spawn(regia_render::PointLight::default()).id()
    }

    /// P-065 acceptance: on a level with 3 lights and 2 meshes, "same kind" from a light selects
    /// the 3 lights (a spot light counts as a light); from a mesh, the 2 meshes. "Same asset"
    /// selects the meshes sharing a path; "same components" separates a mesh with a collider.
    #[test]
    fn select_similar_by_kind_asset_components_and_layer() {
        let mut world = World::new();
        let l1 = light(&mut world);
        let l2 = light(&mut world);
        let l3 = world.spawn(regia_render::SpotLight::default()).id();
        let m1 = world
            .spawn(regia_render::MeshSource::new("assets/rock.glb", ""))
            .id();
        let m2 = world
            .spawn(regia_render::MeshSource::new("assets/tree.glb", ""))
            .id();
        let mut selection = Selection::default();
        selection.select(l1, SelectMode::Replace);
        world.insert_resource(selection);
        let found = select_similar(&mut world, SimilarBy::Kind).expect("something selected");
        let mut lights = vec![l1, l2, l3];
        lights.sort_by_key(|e| e.to_bits());
        assert_eq!(found, lights, "same kind from a light: the 3 lights");
        assert_eq!(world.resource::<Selection>().len(), 3);
        assert_eq!(
            world.resource::<Selection>().primary(),
            Some(l1),
            "the seed stays primary"
        );
        assert_eq!(
            similar_entities(&world, &[m1], SimilarBy::Kind),
            by_bits(vec![m1, m2])
        );
        // Same asset: only the other rock.
        let m3 = world
            .spawn(regia_render::MeshSource::new("assets/rock.glb", ""))
            .id();
        assert_eq!(
            similar_entities(&world, &[m1], SimilarBy::Asset),
            by_bits(vec![m1, m3])
        );
        // Same components: the point lights, not the spot light.
        assert_eq!(
            similar_entities(&world, &[l2], SimilarBy::Components),
            by_bits(vec![l1, l2])
        );
        // Same layer.
        let in_a = |world: &mut World, layers: &[&str]| {
            world
                .spawn(regia_scene::layers::LayerMembership {
                    layers: layers.iter().map(|s| (*s).to_owned()).collect(),
                })
                .id()
        };
        let a1 = in_a(&mut world, &["props"]);
        let a2 = in_a(&mut world, &["props", "night"]);
        let _b1 = in_a(&mut world, &["night"]);
        assert_eq!(
            similar_entities(&world, &[a1], SimilarBy::Layer),
            by_bits(vec![a1, a2]),
            "a shared layer matches"
        );
        // Nothing selected: nothing to compare.
        world.resource_mut::<Selection>().clear();
        assert!(select_similar(&mut world, SimilarBy::Kind).is_none());
        assert_eq!(SimilarBy::parse("asset"), Some(SimilarBy::Asset));
    }

    /// P-074: a click on any descendant of a marked entity selects the nearest marked ancestor;
    /// Ctrl (child pick) keeps the child; an unmarked tree picks the child.
    #[test]
    fn a_selection_root_takes_clicks_on_its_descendants() {
        let mut world = World::new();
        let root = world.spawn(EditorSelectionRoot).id();
        let mid = world.spawn(ChildOf(root)).id();
        let leaf = world.spawn(ChildOf(mid)).id();
        let loose = world.spawn_empty().id();
        assert_eq!(selection_root_of(&world, leaf, false), root);
        assert_eq!(selection_root_of(&world, mid, false), root);
        assert_eq!(selection_root_of(&world, root, false), root);
        assert_eq!(
            selection_root_of(&world, leaf, true),
            leaf,
            "Ctrl+click picks the child"
        );
        assert_eq!(selection_root_of(&world, loose, false), loose);
        world.entity_mut(mid).insert(EditorSelectionRoot);
        assert_eq!(
            selection_root_of(&world, leaf, false),
            mid,
            "the nearest marked ancestor"
        );
    }
}
