// SPDX-License-Identifier: MPL-2.0
//! Asset panels on the editor's one shared history (P-322 part b, `docs/specs/19-editor.md` § 7,
//! ADR-8365): the Theme panel's draft, the UI Animation editor's library, the UI designer's
//! documents and the Vehicle Tuning panel's trailer and aircraft documents.
//!
//! The models edit without a world (the panels, the agent methods and the tests all call them on
//! their resource), so each one keeps a [`SharedHistory`]: every edit is recorded there as a step
//! with the document's state before and after. [`flush`] moves the recorded steps onto the
//! [`UndoStack`] as [`PanelEdit`] commands (a discrete edit is its own entry; the frames of one
//! drag merge into one), after which the model's own undo and redo go through the shared stack.
//!
//! [`flush`] runs at every agent request boundary (the dispatcher's `before` and `idle` hooks, which
//! also run once a frame with no request), so an agent edit is on the stack before the next request
//! reads or navigates it, and a panel's edit by the next frame. Until the first flush a model
//! keeps its private history (a model without an editor around it: unit tests, safe mode).
//!
//! A panel's own Undo button or method (`editor.theme_panel_set { history }`,
//! `editor.ui_anim_edit { op: undo }`, `editor.ui_designer_edit { op: undo }`,
//! `editor.vehicle_tuning_undo { asset: trailer }`) is the shared undo, as for tables and the
//! car ([`before_request`] runs it before the handler, so the answer shows the result).

use std::marker::PhantomData;

use regia_agent::protocol::RpcError;
use regia_ecs::prelude::*;
use serde_json::Value;

use crate::command::{CommandTouch, EditorCommand, HistoryError, UNDO_CAP, UndoStack};

/// How an edit joins the history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepKind {
    /// One step on its own.
    Discrete,
    /// The first edit of a drag: later [`StepKind::Continue`] edits join it.
    Start,
    /// A later frame of the running drag.
    Continue,
}

/// One recorded edit of a model, not yet on the shared stack.
#[derive(Debug, Clone)]
pub(crate) struct Step<S> {
    doc: u64,
    path: String,
    before: S,
    after: S,
    gesture: Option<u64>,
    what: String,
}

/// A model's link to the shared history: what it recorded since the last [`flush`], the stack's
/// depths at that flush, and the undo or redo its panel asked for.
#[derive(Debug, Clone)]
pub(crate) struct SharedHistory<S> {
    /// A flush with an [`UndoStack`] has run: the shared stack holds the history.
    shared: bool,
    /// The shared stack's undo and redo depths at the last flush.
    depths: (usize, usize),
    pending: Vec<Step<S>>,
    /// An undo (`false`) or redo (`true`) the panel asked for, run at the next flush.
    request: Option<bool>,
    /// [`before_request`] decided this undo or redo for the handler: `(redo, ran)`.
    verdict: Option<(bool, bool)>,
    gesture: u64,
}

impl<S> Default for SharedHistory<S> {
    fn default() -> Self {
        Self {
            shared: false,
            depths: (0, 0),
            pending: Vec::new(),
            request: None,
            verdict: None,
            gesture: 0,
        }
    }
}

/// Histories compare equal: they are not part of a document's content (a model that derives
/// `PartialEq` compares what it edits).
impl<S> PartialEq for SharedHistory<S> {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl<S: Clone> SharedHistory<S> {
    /// Whether the shared stack holds this model's history (its own undo list stays empty).
    pub(crate) fn is_shared(&self) -> bool {
        self.shared
    }

    /// Records an edit of document `doc` (saved at `path`) from `before` to `after`; `what` names
    /// the property for the history.
    pub(crate) fn record(&mut self, doc: u64, path: &str, before: &S, after: &S, kind: StepKind, what: &str) {
        if kind == StepKind::Continue
            && let Some(last) = self.pending.last_mut()
            && last.gesture == Some(self.gesture)
            && last.doc == doc
        {
            last.after = after.clone();
            return;
        }
        let gesture = match kind {
            StepKind::Discrete => None,
            StepKind::Start => {
                self.gesture = self.gesture.wrapping_add(1);
                Some(self.gesture)
            }
            StepKind::Continue => Some(self.gesture),
        };
        self.pending.push(Step {
            doc,
            path: path.to_owned(),
            before: before.clone(),
            after: after.clone(),
            gesture,
            what: what.to_owned(),
        });
        if self.pending.len() > UNDO_CAP {
            self.pending.remove(0);
        }
    }

    /// Undo depth of the shared history (with the steps not flushed yet).
    pub(crate) fn undo_depth(&self) -> usize {
        self.depths.0 + self.pending.len()
    }

    /// Redo depth of the shared history.
    pub(crate) fn redo_depth(&self) -> usize {
        if self.pending.is_empty() { self.depths.1 } else { 0 }
    }

    /// The shared undo (`redo == false`) or redo for the model's own button or method: `true` when
    /// it ran or will run at the next flush (it runs only when the next step is this document's),
    /// `false` when there is nothing to step.
    pub(crate) fn navigate(&mut self, redo: bool) -> bool {
        if let Some((decided, ran)) = self.verdict.take()
            && decided == redo
        {
            return ran;
        }
        let available = if redo {
            self.redo_depth()
        } else {
            self.undo_depth()
        };
        if available == 0 {
            return false;
        }
        self.request = Some(redo);
        true
    }
}

/// A model whose edits go on the shared history.
pub(crate) trait SharedDoc: 'static {
    /// The document state a step restores.
    type State: Clone + Send + Sync + 'static;
    /// The panel's name in history descriptions (`Theme`, `UI animation`).
    const PANEL: &'static str;
    /// The models' histories in `world` (the open document and any it keeps open behind it).
    fn histories(world: &mut World) -> Vec<&mut SharedHistory<Self::State>>;
    /// Puts `state` back into the document `doc` (at `path`), wherever it is now.
    fn restore(world: &mut World, doc: u64, path: &str, state: &Self::State);
    /// The open document's identity and history name, if one is open (identity 0: a document
    /// known by its file only).
    fn open_doc(world: &World) -> Option<(u64, String)>;
}

/// One edit of a [`SharedDoc`] on the shared stack.
pub(crate) struct PanelEdit<D: SharedDoc> {
    step: Step<D::State>,
    _doc: PhantomData<fn() -> D>,
}

impl<D: SharedDoc> EditorCommand for PanelEdit<D> {
    fn apply(&mut self, world: &mut World) {
        D::restore(world, self.step.doc, &self.step.path, &self.step.after);
    }
    fn revert(&mut self, world: &mut World) {
        D::restore(world, self.step.doc, &self.step.path, &self.step.before);
    }
    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<Self>() else {
            return false;
        };
        if self.step.gesture.is_some()
            && self.step.gesture == next.step.gesture
            && self.step.doc == next.step.doc
        {
            self.step.after = next.step.after.clone();
            true
        } else {
            false
        }
    }
    fn changes_level_state(&self) -> bool {
        false
    }
    fn touched(&self) -> Vec<CommandTouch> {
        vec![CommandTouch::asset(
            self.step.path.clone(),
            Some(self.step.what.clone()),
        )]
    }
    fn description(&self) -> String {
        format!("{}: {} in {}", D::PANEL, self.step.what, self.step.path)
    }
}

/// Moves `D`'s recorded steps onto the stack and runs the undo or redo its panel asked for.
fn flush_doc<D: SharedDoc>(world: &mut World) {
    let shared = world.contains_resource::<UndoStack>();
    let mut commands: Vec<Box<PanelEdit<D>>> = Vec::new();
    let mut request = None;
    for history in D::histories(world) {
        if !shared {
            continue;
        }
        history.shared = true;
        request = request.or(history.request.take());
        for step in history.pending.drain(..) {
            commands.push(Box::new(PanelEdit {
                step,
                _doc: PhantomData,
            }));
        }
    }
    if !shared {
        return;
    }
    if !commands.is_empty() {
        let mut stack = world.resource_mut::<UndoStack>();
        for command in commands {
            let continuous = command.step.gesture.is_some();
            if !continuous {
                stack.seal();
            }
            stack.push(command);
            if !continuous {
                stack.seal();
            }
        }
    }
    if let Some(redo) = request
        && next_is_own::<D>(world, redo)
    {
        if redo {
            crate::command::redo_world(world);
        } else {
            crate::command::undo_world(world);
        }
    }
    refresh::<D>(world);
}

/// Copies the stack's depths into `D`'s histories.
fn refresh<D: SharedDoc>(world: &mut World) {
    let Some(depths) = world
        .get_resource::<UndoStack>()
        .map(|stack| (stack.undo_depth(), stack.redo_depth()))
    else {
        return;
    };
    for history in D::histories(world) {
        history.depths = depths;
    }
}

/// Moves every shared panel's recorded edits onto the [`UndoStack`] and runs the undo or redo a
/// panel asked for (the dispatcher's request boundaries, every frame).
pub(crate) fn flush(world: &mut World) {
    let _span = tracing::trace_span!(target: "regia_editor", "shared_panel_history").entered();
    flush_doc::<crate::theme_panel::ThemeHistory>(world);
    flush_doc::<crate::ui_anim_doc::UiAnimHistory>(world);
    flush_doc::<crate::ui_designer::DesignHistory>(world);
    flush_doc::<crate::vehicle_tuning::assets::TrailerHistory>(world);
    flush_doc::<crate::vehicle_tuning::assets::AircraftHistory>(world);
}

/// Which panel's own undo or redo route a request is, if any: `(panel, redo)`.
fn panel_history_route(method: &str, params: &Value) -> Option<(&'static str, bool)> {
    let redo_word = |word: Option<&str>| match word {
        Some("undo") => Some(false),
        Some("redo") => Some(true),
        _ => None,
    };
    match method {
        "editor.theme_panel_set" => {
            redo_word(params.get("history").and_then(Value::as_str)).map(|redo| ("theme", redo))
        }
        "editor.ui_anim_edit" => {
            redo_word(params.get("op").and_then(Value::as_str)).map(|redo| ("ui_anim", redo))
        }
        "editor.ui_designer_edit" => {
            redo_word(params.get("op").and_then(Value::as_str)).map(|redo| ("ui_designer", redo))
        }
        "editor.vehicle_tuning_undo" | "editor.vehicle_tuning_redo" => {
            let redo = method.ends_with("redo");
            match params.get("asset").and_then(Value::as_str) {
                Some("trailer") => Some(("trailer", redo)),
                Some("aircraft") => Some(("aircraft", redo)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether the shared stack's next undo (or redo) step is an edit of `D`'s open document: a
/// panel's own Undo and Redo step only its document (the editor's Undo steps everything).
fn next_is_own<D: SharedDoc>(world: &World, redo: bool) -> bool {
    let (Some(stack), Some((doc, path))) = (world.get_resource::<UndoStack>(), D::open_doc(world)) else {
        return false;
    };
    if stack.transaction_label().is_some() {
        return false;
    }
    if redo {
        let prefix = format!("{}: ", D::PANEL);
        stack
            .history()
            .into_iter()
            .find(|entry| !entry.applied)
            .is_some_and(|entry| entry.description.starts_with(&prefix) && entry.documents.contains(&path))
    } else {
        stack
            .top()
            .and_then(|top| (top as &dyn std::any::Any).downcast_ref::<PanelEdit<D>>())
            .is_some_and(|edit| edit.step.doc == doc && (doc != 0 || edit.step.path == path))
    }
}

/// Runs `D`'s own undo or redo route on the shared stack when the next step is its document's,
/// and tells its histories what happened for the handler.
fn route_doc<D: SharedDoc>(world: &mut World, redo: bool) {
    if !D::histories(world).iter().any(|h| h.is_shared()) {
        return;
    }
    let ran = next_is_own::<D>(world, redo);
    if ran {
        if redo {
            crate::command::redo_world(world);
        } else {
            crate::command::undo_world(world);
        }
    }
    refresh::<D>(world);
    for history in D::histories(world) {
        history.verdict = Some((redo, ran));
    }
}

/// The dispatcher's `before` hook part (`undo_agent::transaction_guard`): flushes the edits the
/// last request (or the last frame) recorded, then runs a panel's own undo or redo route on the
/// shared stack before its handler, so the handler answers with the result (or its
/// nothing-to-undo error when the next step is not its document's). While a transaction groups
/// edits, such a route is refused like `editor.undo` (`transaction_active`).
///
/// # Errors
/// `transaction_active` for a panel's undo or redo while a transaction is open.
pub(crate) fn before_request(world: &mut World, method: &str, params: &Value) -> Result<(), RpcError> {
    flush(world);
    let Some((panel, redo)) = panel_history_route(method, params) else {
        return Ok(());
    };
    if world
        .get_resource::<UndoStack>()
        .is_some_and(|stack| stack.transaction_label().is_some())
    {
        return Err(crate::undo_agent::history_error(HistoryError::Active));
    }
    match panel {
        "theme" => route_doc::<crate::theme_panel::ThemeHistory>(world, redo),
        "ui_anim" => route_doc::<crate::ui_anim_doc::UiAnimHistory>(world, redo),
        "ui_designer" => route_doc::<crate::ui_designer::DesignHistory>(world, redo),
        "trailer" => route_doc::<crate::vehicle_tuning::assets::TrailerHistory>(world, redo),
        _ => route_doc::<crate::vehicle_tuning::assets::AircraftHistory>(world, redo),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drag_records_one_step_and_a_discrete_edit_its_own() {
        let mut history = SharedHistory::<u32>::default();
        history.record(1, "a", &0, &1, StepKind::Start, "x");
        history.record(1, "a", &1, &2, StepKind::Continue, "x");
        history.record(1, "a", &2, &3, StepKind::Continue, "x");
        history.record(1, "a", &3, &4, StepKind::Discrete, "y");
        assert_eq!(history.pending.len(), 2);
        assert_eq!((history.pending[0].before, history.pending[0].after), (0, 3));
        assert_eq!(history.pending[1].gesture, None);
        assert_eq!(history.undo_depth(), 2);
    }

    #[test]
    fn navigation_reports_an_empty_history_and_consumes_a_done_step() {
        let mut history = SharedHistory::<u32>::default();
        assert!(!history.navigate(false), "nothing to undo");
        history.verdict = Some((false, true));
        assert!(history.navigate(false), "the step the hook ran");
        assert_eq!(history.request, None, "and nothing queued");
        history.record(1, "a", &0, &1, StepKind::Discrete, "x");
        assert!(history.navigate(false));
        assert_eq!(history.request, Some(false));
        history.verdict = Some((false, false));
        assert!(
            !history.navigate(false),
            "the hook found another document's step next"
        );
    }
}
