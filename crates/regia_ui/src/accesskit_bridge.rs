// SPDX-License-Identifier: MPL-2.0
//! The screen-reader OS bridge, UI half (`accesskit` feature, ADR-1998; gap audit D12).
//!
//! [`AccessKitBackend`] is a [`ScreenReaderBackend`]: each frame the screen-reader sync hands it the
//! accessibility tree ([`crate::a11y::build_a11y_tree`]), and it sends an AccessKit [`TreeUpdate`] to the
//! [`AccessKitBridge`], whose OS adapter (`regia_platform::accessibility`) passes it to AT-SPI, UI
//! Automation or NSAccessibility. The other way, the adapter's action requests come back as
//! [`A11yRequest`]s and run through [`crate::a11y::apply_request`], the path a key press or click
//! takes.
//!
//! - **Stable ids.** A node's AccessKit id is its entity bits ([`A11yNode::id`]), the id
//!   `ui.a11y_tree` and `ui.a11y_act` use. Three synthetic nodes use ids no entity can have (an
//!   entity's low 32 bits are never 0: the index is stored inverted and is never `u32::MAX`): the
//!   window root ([`ROOT_NODE`]) and two live-region announcers ([`POLITE_ANNOUNCER`],
//!   [`ASSERTIVE_ANNOUNCER`]).
//! - **Mapping.** Role ([`role`]), name, description plus usage hint, value (text, and the number
//!   and range of sliders and progress bars), bounds (physical px in the window), disabled,
//!   selected, invalid, checked, expanded, read-only, live politeness, the open dialog of the top
//!   input layer as modal, and the actions the UI's request path accepts ([`actions_for`]).
//!   [`ScreenReader::announce`](crate::a11y::ScreenReader::announce) and live-region changes become a
//!   change of an announcer's label, which the platform reads out.
//! - **Incremental under a budget.** The first update after an assistive technology connects is the
//!   whole tree. After that an update carries only nodes that changed: structural changes (a new node,
//!   a node whose children changed, the modal flag moving, an announcer) always go, property-only
//!   changes fill the slots left after the structural nodes (`budget` minus that count), and the rest
//!   wait for later frames, so a frame where a thousand labels change does not stall. The `ui_a11y_accesskit`
//!   span covers the work.

use std::collections::{BTreeSet, HashMap};

use accesskit::{
    Action, ActionData, ActionRequest, CustomAction, Invalid, Live, Node, NodeId, Rect, Role, Toggled, Tree,
    TreeId, TreeUpdate,
};
use regia_app::prelude::*;
use regia_platform::accessibility::AccessKitBridge;

use crate::a11y::{
    A11yAction, A11yEvent, A11yNode, A11yRequest, A11yRole, A11yTree, PageDirection, Politeness,
    ScreenReader, ScreenReaderBackend,
};

/// The window root's AccessKit id (low 32 bits 0: no entity has it).
pub const ROOT_NODE: u64 = 0;
/// The polite announcer's id: a live region with no bounds whose label is the latest polite
/// announcement.
pub const POLITE_ANNOUNCER: u64 = 1 << 32;
/// The assertive announcer's id.
pub const ASSERTIVE_ANNOUNCER: u64 = 2 << 32;
/// The custom action id of "close" on a dialog (AccessKit has no dismiss action).
pub const CLOSE_ACTION_ID: i32 = 1;
/// Property-only node changes sent per frame at most, by default.
pub const DEFAULT_NODE_BUDGET: usize = 256;
/// The toolkit name in the tree.
pub const TOOLKIT_NAME: &str = "Regia";

/// The AccessKit role of an engine role.
pub fn role(role: A11yRole) -> Role {
    match role {
        // A `UiRoot` is a pane inside the OS window (the synthetic root is the window).
        A11yRole::Window => Role::Pane,
        A11yRole::Group => Role::Group,
        A11yRole::Button => Role::Button,
        A11yRole::CheckBox => Role::CheckBox,
        A11yRole::Slider => Role::Slider,
        A11yRole::ProgressBar => Role::ProgressIndicator,
        A11yRole::TextInput => Role::TextInput,
        A11yRole::ComboBox => Role::ComboBox,
        A11yRole::TabList => Role::TabList,
        A11yRole::Dialog => Role::Dialog,
        A11yRole::List => Role::List,
        A11yRole::ScrollView => Role::ScrollView,
        A11yRole::Label => Role::Label,
        A11yRole::Image => Role::Image,
        A11yRole::Menu => Role::Menu,
        A11yRole::Heading => Role::Heading,
        A11yRole::Region => Role::Region,
        A11yRole::Status => Role::Status,
        A11yRole::Alert => Role::Alert,
    }
}

/// The actions the UI's request path accepts for `node` ([`crate::a11y::apply_request`]); none for a
/// disabled or inert node. A text field advertises `set_value` only when it is editable; a dialog
/// advertises the custom action [`CLOSE_ACTION_ID`].
pub fn actions_for(node: &A11yNode) -> Vec<Action> {
    if node.states.disabled || node.states.inert {
        return Vec::new();
    }
    let mut actions = Vec::new();
    let focusable = node.states.focusable;
    match node.role() {
        A11yRole::Slider => actions.extend([Action::Increment, Action::Decrement]),
        A11yRole::TextInput => {
            if !node.states.read_only {
                actions.push(Action::SetValue);
            }
        }
        A11yRole::Button | A11yRole::CheckBox | A11yRole::ComboBox => actions.push(Action::Click),
        A11yRole::Dialog => actions.push(Action::CustomAction),
        A11yRole::ScrollView | A11yRole::List => actions.extend([
            Action::ScrollUp,
            Action::ScrollDown,
            Action::ScrollLeft,
            Action::ScrollRight,
        ]),
        _ if focusable => actions.push(Action::Click),
        _ => {}
    }
    if focusable {
        actions.push(Action::Focus);
    }
    if node.parent.is_some() {
        actions.push(Action::ScrollIntoView);
    }
    actions
}

/// The node's bounds, or `None` for an empty, negative or non-finite layout rect (a degenerate
/// layout must not reach the platform adapters; audit wave 5, accesskit M4).
fn rect(bounds: [f32; 4]) -> Option<Rect> {
    let [x, y, w, h] = bounds.map(f64::from);
    let sane = bounds.iter().all(|v| v.is_finite()) && w >= 0.0 && h >= 0.0;
    (sane && (w > 0.0 || h > 0.0)).then_some(Rect {
        x0: x,
        y0: y,
        x1: x + w,
        y1: y + h,
    })
}

/// The most characters of one string (label, description, value, announcement) handed to the
/// platform; a longer one is cut there (audit wave 5, accesskit M3).
pub const MAX_PLATFORM_TEXT_CHARS: usize = 4096;

/// `text` as it may go to the OS accessibility bus: control characters other than line breaks and
/// tabs become spaces, and it is cut to [`MAX_PLATFORM_TEXT_CHARS`]. Borrowed when already clean.
pub fn platform_text(text: &str) -> std::borrow::Cow<'_, str> {
    let bad = |c: char| c.is_control() && c != '\n' && c != '\t';
    let long = text.len() > MAX_PLATFORM_TEXT_CHARS && text.chars().nth(MAX_PLATFORM_TEXT_CHARS).is_some();
    if !long && !text.contains(bad) {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(
        text.chars()
            .take(MAX_PLATFORM_TEXT_CHARS)
            .map(|c| if bad(c) { ' ' } else { c })
            .collect(),
    )
}

/// The AccessKit node of `node`; `modal` marks the dialog of the top input layer.
pub fn build_node(node: &A11yNode, modal: bool) -> Node {
    let mut out = Node::new(role(node.role()));
    out.set_children(node.children.iter().map(|c| NodeId(*c)).collect::<Vec<_>>());
    if !node.name.is_empty() {
        out.set_label(platform_text(&node.name));
    }
    // AccessKit has no usage hint: it is read after the description.
    let description = match (node.description.is_empty(), node.hint.is_empty()) {
        (true, true) => String::new(),
        (false, true) => node.description.clone(),
        (true, false) => node.hint.clone(),
        (false, false) => format!("{}. {}", node.description, node.hint),
    };
    if !description.is_empty() {
        out.set_description(platform_text(&description));
    }
    if let Some(value) = node.value.as_ref().filter(|v| !v.is_empty()) {
        out.set_value(platform_text(value));
    }
    if let Some(range) = node.range {
        out.set_numeric_value(range.value);
        out.set_min_numeric_value(range.min);
        out.set_max_numeric_value(range.max);
        if range.step > 0.0 {
            out.set_numeric_value_step(range.step);
        }
    }
    if let Some(bounds) = rect(node.bounds) {
        out.set_bounds(bounds);
    }
    let states = &node.states;
    if states.disabled {
        out.set_disabled();
    }
    if states.selected {
        out.set_selected(true);
    }
    if states.invalid {
        out.set_invalid(Invalid::True);
    }
    if let Some(checked) = states.checked {
        out.set_toggled(Toggled::from(checked));
    }
    if let Some(expanded) = states.expanded {
        out.set_expanded(expanded);
    }
    if states.read_only {
        out.set_read_only();
    }
    if modal {
        out.set_modal();
    }
    match node.live {
        Politeness::Off => {}
        Politeness::Polite => out.set_live(Live::Polite),
        Politeness::Assertive => out.set_live(Live::Assertive),
    }
    for action in actions_for(node) {
        out.add_action(action);
    }
    if node.role() == A11yRole::Dialog && !states.disabled && !states.inert {
        out.set_custom_actions(vec![CustomAction {
            id: CLOSE_ACTION_ID,
            description: "Close".into(),
        }]);
    }
    out
}

/// The engine request an AccessKit action request asks for; `None` for an action the UI has no
/// path for (it is counted in [`AccessKitBackend::unmapped`]).
///
/// `click` presses, `focus` focuses, `increment` / `decrement` raise and lower, `set_value` and
/// `replace_selected_text` with text set the text. A `set_value` whose data is a number is not
/// mapped (the request can still be queued, and `maps_to` is null). `custom_action`
/// [`CLOSE_ACTION_ID`] closes, `scroll_into_view` scrolls the node into view, `scroll_up` /
/// `down` / `left` / `right` page.
pub fn request_from_action(request: &ActionRequest) -> Option<A11yRequest> {
    // This bridge publishes one tree, the root one: a request naming another tree is not ours
    // (audit wave 5, accesskit L1).
    if request.target_tree != TreeId::ROOT {
        return None;
    }
    let action = match (request.action, &request.data) {
        (Action::Click, _) => A11yAction::Press,
        (Action::Focus, _) => A11yAction::Focus,
        (Action::Increment, _) => A11yAction::Raise,
        (Action::Decrement, _) => A11yAction::Lower,
        // Text past the cap is not copied at all (it would be refused at apply time anyway).
        (Action::SetValue | Action::ReplaceSelectedText, Some(ActionData::Value(text)))
            if text.chars().nth(crate::a11y::MAX_SET_TEXT_CHARS).is_none() =>
        {
            A11yAction::SetText(text.to_string())
        }
        (Action::CustomAction, Some(ActionData::CustomAction(CLOSE_ACTION_ID))) => A11yAction::Close,
        (Action::ScrollIntoView, _) => A11yAction::ScrollIntoView,
        (Action::ScrollUp, _) => A11yAction::Page(PageDirection::Up),
        (Action::ScrollDown, _) => A11yAction::Page(PageDirection::Down),
        (Action::ScrollLeft, _) => A11yAction::Page(PageDirection::Left),
        (Action::ScrollRight, _) => A11yAction::Page(PageDirection::Right),
        _ => return None,
    };
    let node = request.target_node.0;
    (node != ROOT_NODE && node != POLITE_ANNOUNCER && node != ASSERTIVE_ANNOUNCER)
        .then_some(A11yRequest { node, action })
}

/// Counters of the last update, for the agent and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateStats {
    /// Nodes in the last update.
    pub last_nodes: usize,
    /// Of those, structural (always sent).
    pub last_structural: usize,
    /// Property-only changes left for later frames after the last update.
    pub deferred: usize,
    /// The largest update so far (nodes).
    pub max_nodes: usize,
}

/// What was sent to the platform, so the next update carries only what changed.
#[derive(Debug)]
pub struct AccessKitTreeState {
    sent: HashMap<u64, A11yNode>,
    root_children: Vec<u64>,
    modal: Option<u64>,
    focus: Option<NodeId>,
    announcers: [String; 2],
    deferred: BTreeSet<u64>,
    /// Cap for one update. Structural nodes are always sent, including past this cap.
    /// Property-only slots that frame are this value minus the structural count.
    pub budget: usize,
    /// Counters.
    pub stats: UpdateStats,
}

impl Default for AccessKitTreeState {
    fn default() -> Self {
        Self::new(DEFAULT_NODE_BUDGET)
    }
}

/// The dialog flagged modal: the top input layer's root, when it is a dialog in the tree.
fn modal_of(tree: &A11yTree) -> Option<u64> {
    tree.top_layer
        .filter(|id| tree.get(*id).is_some_and(|n| n.role() == A11yRole::Dialog))
}

fn root_children(tree: &A11yTree) -> Vec<u64> {
    tree.nodes
        .iter()
        .filter(|n| n.parent.is_none())
        .map(|n| n.id)
        .collect()
}

fn announcer(id: u64, live: Live, text: &str) -> (NodeId, Node) {
    let mut node = Node::new(Role::Label);
    node.set_live(live);
    if !text.is_empty() {
        node.set_label(text);
    }
    (NodeId(id), node)
}

impl AccessKitTreeState {
    /// A state that sends at most `budget` property-only changes per update.
    pub fn new(budget: usize) -> Self {
        Self {
            sent: HashMap::new(),
            root_children: Vec::new(),
            modal: None,
            focus: None,
            announcers: [String::new(), String::new()],
            deferred: BTreeSet::new(),
            budget,
            stats: UpdateStats::default(),
        }
    }

    /// Whether property changes wait for a later frame.
    pub fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }

    fn root_node(&self) -> Node {
        let mut root = Node::new(Role::Window);
        let mut children: Vec<NodeId> = self.root_children.iter().map(|c| NodeId(*c)).collect();
        children.extend([NodeId(POLITE_ANNOUNCER), NodeId(ASSERTIVE_ANNOUNCER)]);
        root.set_children(children);
        root
    }

    /// New announcer texts from this frame's announcements: one label per politeness, the frame's
    /// texts joined. The same text again gets a zero-width space toggled on, so the label changes and
    /// the platform reads it again.
    fn announce(&mut self, events: &[A11yEvent]) -> [bool; 2] {
        let mut texts: [Vec<&str>; 2] = [Vec::new(), Vec::new()];
        for event in events {
            if let A11yEvent::Announcement { text, politeness } = event {
                let slot = usize::from(*politeness == Politeness::Assertive);
                texts[slot].push(text.as_str());
            }
        }
        let mut changed = [false; 2];
        for (slot, parts) in texts.iter().enumerate() {
            if parts.is_empty() {
                continue;
            }
            let mut text = platform_text(&parts.join(". ")).into_owned();
            if self.announcers[slot].trim_end_matches('\u{200B}') == text
                && !self.announcers[slot].ends_with('\u{200B}')
            {
                text.push('\u{200B}');
            }
            self.announcers[slot] = text;
            changed[slot] = true;
        }
        changed
    }

    fn announcer_nodes(&self, which: [bool; 2]) -> Vec<(NodeId, Node)> {
        let mut out = Vec::new();
        if which[0] {
            out.push(announcer(POLITE_ANNOUNCER, Live::Polite, &self.announcers[0]));
        }
        if which[1] {
            out.push(announcer(
                ASSERTIVE_ANNOUNCER,
                Live::Assertive,
                &self.announcers[1],
            ));
        }
        out
    }

    fn focus_of(tree: &A11yTree) -> NodeId {
        NodeId(tree.focus.unwrap_or(ROOT_NODE))
    }

    /// The whole tree (what an adapter needs when assistive technology connects).
    pub fn full_update(&mut self, tree: &A11yTree, events: &[A11yEvent]) -> TreeUpdate {
        self.announce(events);
        self.root_children = root_children(tree);
        self.modal = modal_of(tree);
        let mut nodes = Vec::with_capacity(tree.nodes.len() + 3);
        nodes.push((NodeId(ROOT_NODE), self.root_node()));
        for node in &tree.nodes {
            nodes.push((NodeId(node.id), build_node(node, self.modal == Some(node.id))));
        }
        nodes.extend(self.announcer_nodes([true, true]));
        self.sent = tree.nodes.iter().map(|n| (n.id, n.clone())).collect();
        self.deferred.clear();
        let focus = Self::focus_of(tree);
        self.focus = Some(focus);
        self.note(nodes.len(), nodes.len());
        TreeUpdate {
            nodes,
            tree: Some(Tree {
                root: NodeId(ROOT_NODE),
                toolkit_name: Some(TOOLKIT_NAME.to_owned()),
                toolkit_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            tree_id: TreeId::ROOT,
            focus,
        }
    }

    fn note(&mut self, nodes: usize, structural: usize) {
        self.stats.last_nodes = nodes;
        self.stats.last_structural = structural;
        self.stats.deferred = self.deferred.len();
        self.stats.max_nodes = self.stats.max_nodes.max(nodes);
    }

    /// Only what changed since the last update; `None` when nothing did. Must follow a
    /// [`Self::full_update`].
    pub fn incremental_update(&mut self, tree: &A11yTree, events: &[A11yEvent]) -> Option<TreeUpdate> {
        let announced = self.announce(events);
        let modal = modal_of(tree);
        let mut structural: Vec<&A11yNode> = Vec::new();
        let mut changed: Vec<u64> = Vec::new();
        for node in &tree.nodes {
            let modal_moved = self.modal != modal && (Some(node.id) == self.modal || Some(node.id) == modal);
            match self.sent.get(&node.id) {
                None => structural.push(node),
                Some(old) if old.children != node.children || modal_moved => structural.push(node),
                Some(old) if old != node => changed.push(node.id),
                Some(_) => {}
            }
        }
        // Deferred changes that are still pending go first (oldest ids first), then this frame's.
        let by_id: HashMap<u64, &A11yNode> = tree.nodes.iter().map(|n| (n.id, n)).collect();
        let structural_ids: BTreeSet<u64> = structural.iter().map(|n| n.id).collect();
        let mut pending: Vec<u64> = self
            .deferred
            .iter()
            .copied()
            .filter(|id| {
                !structural_ids.contains(id)
                    && by_id
                        .get(id)
                        .is_some_and(|n| self.sent.get(id).is_some_and(|old| old != *n))
            })
            .collect();
        for id in changed {
            if !self.deferred.contains(&id) {
                pending.push(id);
            }
        }
        let slots = self.budget.saturating_sub(structural.len());
        let (now, later) = pending.split_at(pending.len().min(slots));
        self.deferred = later.iter().copied().collect();

        let new_root_children = root_children(tree);
        let root_changed = new_root_children != self.root_children;
        self.root_children = new_root_children;
        self.modal = modal;
        let focus = Self::focus_of(tree);
        let focus_changed = self.focus != Some(focus);
        self.focus = Some(focus);

        let mut nodes: Vec<(NodeId, Node)> = Vec::with_capacity(structural.len() + now.len() + 3);
        if root_changed {
            nodes.push((NodeId(ROOT_NODE), self.root_node()));
        }
        for node in &structural {
            nodes.push((NodeId(node.id), build_node(node, modal == Some(node.id))));
        }
        for id in now {
            if let Some(node) = by_id.get(id) {
                nodes.push((NodeId(*id), build_node(node, modal == Some(*id))));
            }
        }
        nodes.extend(self.announcer_nodes(announced));
        let forced = structural.len() + usize::from(root_changed) + announced.iter().filter(|a| **a).count();
        // Remember what went; deferred nodes keep their old version so they still differ next frame.
        self.sent.retain(|id, _| by_id.contains_key(id));
        for node in structural {
            self.sent.insert(node.id, node.clone());
        }
        for id in now {
            if let Some(node) = by_id.get(id) {
                self.sent.insert(*id, (*node).clone());
            }
        }
        self.note(nodes.len(), forced);
        if nodes.is_empty() && !focus_changed {
            return None;
        }
        Some(TreeUpdate {
            nodes,
            tree: None,
            tree_id: TreeId::ROOT,
            focus,
        })
    }
}

/// The screen-reader backend over the AccessKit bridge.
#[derive(Debug)]
pub struct AccessKitBackend {
    bridge: AccessKitBridge,
    state: AccessKitTreeState,
    /// Action requests the UI has no path for (tooltips, text selection, ...).
    pub unmapped: u64,
    /// Updates the bridge refused (a mapping bug: it is logged and the next update is a full tree).
    pub refused: u64,
}

impl AccessKitBackend {
    /// A backend over `bridge`, with the default budget.
    pub fn new(bridge: AccessKitBridge) -> Self {
        Self::with_budget(bridge, DEFAULT_NODE_BUDGET)
    }

    /// A backend that sends every structural node, and at most `budget` minus that
    /// frame's structural count property-only nodes. A `budget` of 0 is raised to 1.
    pub fn with_budget(bridge: AccessKitBridge, budget: usize) -> Self {
        Self {
            bridge,
            state: AccessKitTreeState::new(budget.max(1)),
            unmapped: 0,
            refused: 0,
        }
    }

    /// The bridge.
    pub fn bridge(&self) -> &AccessKitBridge {
        &self.bridge
    }

    /// The last update's counters.
    pub fn update_stats(&self) -> UpdateStats {
        self.state.stats
    }
}

impl ScreenReaderBackend for AccessKitBackend {
    fn name(&self) -> &'static str {
        "accesskit"
    }

    fn is_active(&self) -> bool {
        self.bridge.is_active()
    }

    fn tree_updated(&mut self, tree: &A11yTree, events: &[A11yEvent]) {
        let full = self.bridge.needs_full_tree();
        let _span = tracing::trace_span!("ui_a11y_accesskit", nodes = tree.nodes.len(), full).entered();
        let update = if full {
            Some(self.state.full_update(tree, events))
        } else {
            self.state.incremental_update(tree, events)
        };
        if let Some(update) = update
            && let Err(error) = self.bridge.push_update(update)
        {
            self.refused += 1;
            tracing::warn!(target: "regia_ui", "screen-reader bridge refused an update: {error}");
        }
    }

    fn recent_events(&self) -> Vec<String> {
        let s = self.bridge.stats();
        let u = self.state.stats;
        vec![format!(
            "accesskit adapter={} active={} updates={} full={} nodes={} last={} deferred={} actions={} unmapped={}",
            self.bridge.adapter_name().unwrap_or("none"),
            self.bridge.is_active(),
            s.updates,
            s.full_updates,
            s.nodes_sent,
            u.last_nodes,
            u.deferred,
            s.actions,
            self.unmapped,
        )]
    }

    fn drain_requests(&mut self) -> Vec<A11yRequest> {
        let mut out = Vec::new();
        for request in self.bridge.take_actions() {
            match request_from_action(&request) {
                Some(mapped) => out.push(mapped),
                None => self.unmapped += 1,
            }
        }
        out
    }

    fn wants_sync(&self) -> bool {
        self.bridge.needs_full_tree() || self.state.has_deferred()
    }
}

/// Installs the bridge (ADR-1998): an [`AccessKitBridge`] resource and a [`ScreenReader`] over an
/// [`AccessKitBackend`], unless the host inserted its own [`ScreenReader`] (then only a bridge the
/// host inserted too is kept). `REGIA_SCREEN_READER=1` marks the bridge active with no assistive
/// technology (headless runs and tests export the tree to the platform tree anyway).
pub fn install(app: &mut App) {
    if app.world().contains_resource::<ScreenReader>() {
        return;
    }
    let bridge = app
        .world()
        .get_resource::<AccessKitBridge>()
        .cloned()
        .unwrap_or_default();
    if regia_core::brand::env_var("REGIA_SCREEN_READER").is_ok_and(|v| v == "1") {
        bridge.activate();
    }
    app.insert_resource(bridge.clone());
    app.insert_resource(ScreenReader::with_backend(Box::new(AccessKitBackend::new(
        bridge,
    ))));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a11y::{A11yRange, A11yStates};

    fn node(id: u64, parent: Option<u64>, role: A11yRole, name: &str) -> A11yNode {
        A11yNode {
            id,
            parent,
            role: Some(role),
            name: name.to_owned(),
            ..A11yNode::default()
        }
    }

    /// Audit wave 5 (accesskit M3): control characters other than line breaks and tabs become
    /// spaces and long text is cut at `MAX_PLATFORM_TEXT_CHARS`; clean text is passed as is.
    #[test]
    fn platform_text_strips_controls_and_caps_length() {
        assert!(matches!(
            platform_text("Start\ngame\t1"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(platform_text("a\u{0}b\u{7}c\u{1b}[31m"), "a b c [31m");
        let long = "é".repeat(MAX_PLATFORM_TEXT_CHARS + 5);
        assert_eq!(platform_text(&long).chars().count(), MAX_PLATFORM_TEXT_CHARS);
        let n = A11yNode {
            description: "x\u{7}".to_owned(),
            value: Some("v\u{0}".to_owned()),
            ..node(7, None, A11yRole::TextInput, "n\u{8}")
        };
        let out = build_node(&n, false);
        assert_eq!(out.label(), Some("n "));
        assert_eq!(out.description(), Some("x "));
        assert_eq!(out.value(), Some("v "));
    }

    /// Audit wave 5 (accesskit M4): non-finite, negative or empty bounds export no rect, and a
    /// well-formed rect still does.
    #[test]
    fn degenerate_bounds_are_not_exported() {
        for bounds in [
            [0.0, 0.0, f32::NAN, 10.0],
            [f32::INFINITY, 0.0, 10.0, 10.0],
            [0.0, 0.0, -5.0, 10.0],
            [0.0, 0.0, 0.0, 0.0],
        ] {
            let n = A11yNode {
                bounds,
                ..node(7, None, A11yRole::Button, "b")
            };
            assert_eq!(build_node(&n, false).bounds(), None, "{bounds:?}");
        }
        let n = A11yNode {
            bounds: [1.0, 2.0, 3.0, 4.0],
            ..node(7, None, A11yRole::Button, "b")
        };
        assert_eq!(
            build_node(&n, false).bounds(),
            Some(Rect {
                x0: 1.0,
                y0: 2.0,
                x1: 4.0,
                y1: 6.0
            })
        );
    }

    #[test]
    fn every_role_maps_and_synthetic_ids_cannot_be_entities() {
        let roles: std::collections::HashSet<Role> = A11yRole::ALL.into_iter().map(role).collect();
        assert_eq!(
            roles.len(),
            A11yRole::ALL.len(),
            "two engine roles share an AccessKit role"
        );
        for id in [ROOT_NODE, POLITE_ANNOUNCER, ASSERTIVE_ANNOUNCER] {
            assert_eq!(id & 0xFFFF_FFFF, 0);
            assert!(
                Entity::try_from_bits(id).is_none(),
                "{id:#x} decodes to an entity"
            );
        }
    }

    #[test]
    fn a_slider_carries_its_range_and_step_actions() {
        let mut slider = node(5, Some(1), A11yRole::Slider, "Volume");
        slider.value = Some("50".to_owned());
        slider.range = Some(A11yRange {
            value: 50.0,
            min: 0.0,
            max: 100.0,
            step: 5.0,
        });
        slider.bounds = [10.0, 20.0, 200.0, 24.0];
        let built = build_node(&slider, false);
        assert_eq!(built.role(), Role::Slider);
        assert_eq!(built.label(), Some("Volume"));
        assert_eq!(built.numeric_value(), Some(50.0));
        assert_eq!(built.numeric_value_step(), Some(5.0));
        assert_eq!(
            built.bounds().map(|r| (r.x0, r.y0, r.x1, r.y1)),
            Some((10.0, 20.0, 210.0, 44.0))
        );
        assert!(built.supports_action(Action::Increment) && built.supports_action(Action::Decrement));
        assert!(!built.supports_action(Action::Click));
    }

    #[test]
    fn disabled_and_inert_nodes_offer_no_actions() {
        let mut button = node(5, Some(1), A11yRole::Button, "Start");
        button.states = A11yStates {
            focusable: true,
            ..A11yStates::default()
        };
        assert_eq!(
            actions_for(&button),
            vec![Action::Click, Action::Focus, Action::ScrollIntoView]
        );
        button.states.disabled = true;
        assert!(actions_for(&button).is_empty());
        assert!(build_node(&button, false).is_disabled());
        button.states.disabled = false;
        button.states.inert = true;
        assert!(actions_for(&button).is_empty());
    }

    #[test]
    fn a_read_only_field_offers_no_set_value() {
        let mut field = node(5, Some(1), A11yRole::TextInput, "Name");
        assert!(actions_for(&field).contains(&Action::SetValue));
        field.states.read_only = true;
        assert!(!actions_for(&field).contains(&Action::SetValue));
        assert!(build_node(&field, false).is_read_only());
    }

    /// Audit wave 5 (accesskit L1, M2): a request for another tree, or a set-value text past
    /// `MAX_SET_TEXT_CHARS`, maps to nothing; text at the cap still maps.
    #[test]
    fn foreign_trees_and_oversized_text_are_unmapped() {
        let set = |tree, text: String| ActionRequest {
            action: Action::SetValue,
            target_tree: tree,
            target_node: NodeId(7),
            data: Some(ActionData::Value(text.into())),
        };
        let other = TreeId(accesskit::Uuid::from_u128(7));
        assert_eq!(request_from_action(&set(other, "Ada".to_owned())), None);
        let cap = crate::a11y::MAX_SET_TEXT_CHARS;
        assert_eq!(request_from_action(&set(TreeId::ROOT, "x".repeat(cap + 1))), None);
        assert_eq!(
            request_from_action(&set(TreeId::ROOT, "x".repeat(cap))).map(|r| r.action),
            Some(A11yAction::SetText("x".repeat(cap)))
        );
    }

    #[test]
    fn action_requests_map_to_the_ui_request_path() {
        let req = |action, data| ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: NodeId(7),
            data,
        };
        let cases = [
            (req(Action::Click, None), Some(A11yAction::Press)),
            (req(Action::Focus, None), Some(A11yAction::Focus)),
            (req(Action::Increment, None), Some(A11yAction::Raise)),
            (req(Action::Decrement, None), Some(A11yAction::Lower)),
            (
                req(Action::SetValue, Some(ActionData::Value("Ada".into()))),
                Some(A11yAction::SetText("Ada".to_owned())),
            ),
            (req(Action::SetValue, Some(ActionData::NumericValue(3.0))), None),
            (
                req(
                    Action::CustomAction,
                    Some(ActionData::CustomAction(CLOSE_ACTION_ID)),
                ),
                Some(A11yAction::Close),
            ),
            (req(Action::CustomAction, Some(ActionData::CustomAction(9))), None),
            (
                req(Action::ScrollDown, None),
                Some(A11yAction::Page(PageDirection::Down)),
            ),
            (
                req(Action::ScrollIntoView, None),
                Some(A11yAction::ScrollIntoView),
            ),
            (req(Action::ShowTooltip, None), None),
        ];
        for (request, expected) in cases {
            assert_eq!(
                request_from_action(&request).map(|r| r.action),
                expected,
                "{request:?}"
            );
        }
        let mut on_root = req(Action::Click, None);
        on_root.target_node = NodeId(ROOT_NODE);
        assert_eq!(request_from_action(&on_root), None);
    }

    fn tree(nodes: Vec<A11yNode>) -> A11yTree {
        A11yTree {
            nodes,
            focus: None,
            top_layer: None,
        }
    }

    #[test]
    fn an_unchanged_tree_sends_nothing_and_a_label_change_one_node() {
        let mut root = node(1, None, A11yRole::Window, "");
        root.children = vec![2, 3];
        let t1 = tree(vec![
            root.clone(),
            node(2, Some(1), A11yRole::Button, "Start"),
            node(3, Some(1), A11yRole::Button, "Quit"),
        ]);
        let mut state = AccessKitTreeState::new(8);
        let full = state.full_update(&t1, &[]);
        // Window root, three nodes, two announcers.
        assert_eq!(full.nodes.len(), 6);
        assert!(state.incremental_update(&t1, &[]).is_none());
        let mut t2 = t1.clone();
        t2.nodes[2].name = "Exit".to_owned();
        let update = state.incremental_update(&t2, &[]).expect("a change");
        assert_eq!(update.nodes.len(), 1);
        assert_eq!(update.nodes[0].0, NodeId(3));
        assert_eq!(update.nodes[0].1.label(), Some("Exit"));
    }

    #[test]
    fn the_same_announcement_twice_changes_the_label() {
        let t = tree(vec![node(1, None, A11yRole::Window, "")]);
        let mut state = AccessKitTreeState::new(8);
        let say = |text: &str| A11yEvent::Announcement {
            text: text.to_owned(),
            politeness: Politeness::Polite,
        };
        state.full_update(&t, &[]);
        let first = state.incremental_update(&t, &[say("Saved")]).expect("announced");
        let second = state
            .incremental_update(&t, &[say("Saved")])
            .expect("announced again");
        let third = state.incremental_update(&t, &[say("Saved")]).expect("and again");
        let label = |u: &TreeUpdate| u.nodes[0].1.label().map(str::to_owned);
        assert_eq!(first.nodes[0].0, NodeId(POLITE_ANNOUNCER));
        assert_ne!(label(&first), label(&second));
        assert_ne!(label(&second), label(&third));
        assert_eq!(
            label(&first).as_deref().map(|l| l.trim_end_matches('\u{200B}')),
            Some("Saved")
        );
    }
}
