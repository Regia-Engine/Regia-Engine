// SPDX-License-Identifier: MPL-2.0
//! The screen-reader OS bridge, platform half (`accesskit` feature, ADR-1998; the bridge and the
//! adapters alone, without the runner's hooks, are the `accesskit-adapters` feature, ADR-7795).
//!
//! `regia_ui` turns its accessibility tree into AccessKit [`TreeUpdate`]s and hands them to an
//! [`AccessKitBridge`] (a cloneable resource). This module owns the other end: one AccessKit adapter
//! per OS window, created by the runner behind the platform seam (`platform/<os>.rs`): AT-SPI over
//! D-Bus on Linux (`accesskit_unix`), UI Automation on Windows (`accesskit_windows`, subclassing the
//! window before it is first shown), NSAccessibility on macOS (`accesskit_macos`), TalkBack on
//! Android (`accesskit_android` on the activity's surface view, ADR-3454). Each frame,
//! `drive_accesskit_adapter` (`Last`) passes the queued updates and the window's focus state to the
//! adapter on the main thread.
//!
//! The adapters call back from their own threads: [`BridgeHandler`] records an activation (an
//! assistive technology connected: the next update must be a full tree), a deactivation, and every
//! action request (click, focus, increment, set value, ...) in the bridge. `regia_ui` drains the
//! requests in `PreUpdate` and routes them through the UI's input path.
//!
//! The bridge also keeps [`PlatformTree`], the tree as the adapter has it after every update it
//! was sent, checked the way the adapters check it (every child and the focus must exist). The
//! agent's `ui.a11y_platform_tree` dumps it, and tests read it instead of a screen reader.
//!
//! `REGIA_ACCESSKIT=0` keeps the OS adapter off (the bridge and its mirror still work).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

pub use accesskit;
use accesskit::{Action, ActionRequest, Node, NodeId, TreeUpdate};
use regia_app::prelude::*;

/// How many updates wait for the adapter at most. When the adapter falls this far behind (it was
/// not driven), the queue is dropped and the next update is a full tree.
pub const MAX_QUEUED_UPDATES: usize = 16;
/// How many action requests wait for the UI at most (older ones are dropped first).
pub const MAX_QUEUED_ACTIONS: usize = 64;

/// Every AccessKit action, in declaration order, with the protocol name the agent uses.
pub const ACTION_NAMES: [(Action, &str); 22] = [
    (Action::Click, "click"),
    (Action::Focus, "focus"),
    (Action::Blur, "blur"),
    (Action::Collapse, "collapse"),
    (Action::Expand, "expand"),
    (Action::CustomAction, "custom_action"),
    (Action::Decrement, "decrement"),
    (Action::Increment, "increment"),
    (Action::HideTooltip, "hide_tooltip"),
    (Action::ShowTooltip, "show_tooltip"),
    (Action::ReplaceSelectedText, "replace_selected_text"),
    (Action::ScrollDown, "scroll_down"),
    (Action::ScrollLeft, "scroll_left"),
    (Action::ScrollRight, "scroll_right"),
    (Action::ScrollUp, "scroll_up"),
    (Action::ScrollIntoView, "scroll_into_view"),
    (Action::ScrollToPoint, "scroll_to_point"),
    (Action::SetScrollOffset, "set_scroll_offset"),
    (Action::SetTextSelection, "set_text_selection"),
    (
        Action::SetSequentialFocusNavigationStartingPoint,
        "set_sequential_focus_navigation_starting_point",
    ),
    (Action::SetValue, "set_value"),
    (Action::ShowContextMenu, "show_context_menu"),
];

/// The protocol name of `action` (`snake_case`).
pub fn action_name(action: Action) -> &'static str {
    ACTION_NAMES
        .iter()
        .find(|(a, _)| *a == action)
        .map_or("unknown", |(_, name)| name)
}

/// Parses [`action_name`].
pub fn parse_action(name: &str) -> Option<Action> {
    ACTION_NAMES.iter().find(|(_, n)| *n == name).map(|(a, _)| *a)
}

/// Why the Android adapter refused to attach (ADR-3454). Checked before any JNI call that AccessKit
/// would panic on, and unit-tested on every host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AndroidAttachBlock {
    /// No activity handle (`regia::run_android` did not run, or it has already dropped).
    NoActivity,
    /// `GameActivity.mSurfaceView` is missing or null, so there is no view to inject into.
    NoSurfaceView,
    /// The view already has an accessibility delegate. Injecting another one panics inside AccessKit.
    DelegateAlreadySet,
}

impl AndroidAttachBlock {
    /// Stable text stored in `adapter_error` and logged when attach stops.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoActivity => "no Android activity (start the app with regia::run_android)",
            Self::NoSurfaceView => "GameActivity.mSurfaceView is missing or null",
            Self::DelegateAlreadySet => "the view already has an accessibility delegate",
        }
    }
}

impl std::fmt::Display for AndroidAttachBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

/// Whether the Android adapter may call `InjectingAdapter::new`. A missing activity wins over a
/// missing view, which wins over a delegate that is already set.
pub fn android_attach_decision(
    has_activity: bool,
    surface_view_present: bool,
    delegate_already_set: bool,
) -> Result<(), AndroidAttachBlock> {
    if !has_activity {
        return Err(AndroidAttachBlock::NoActivity);
    }
    if !surface_view_present {
        return Err(AndroidAttachBlock::NoSurfaceView);
    }
    if delegate_already_set {
        return Err(AndroidAttachBlock::DelegateAlreadySet);
    }
    Ok(())
}

/// Android row of `ui.a11y_platform_tree` and `ui.a11y_platform_connect` (ADR-3454). Built on every
/// host: a desktop process reports `not_android`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AndroidBridgeStatus {
    /// This process is Android.
    pub os: bool,
    /// This binary includes the Android adapter (the `accesskit` feature on Android).
    pub registered: bool,
    /// The adapter is attached to the activity's surface view.
    pub attached: bool,
    /// `not_android`, `pending`, `attached` or `disabled`.
    pub status: &'static str,
    /// One sentence for the agent page. The concrete JNI failure stays in `adapter_error`.
    pub detail: &'static str,
}

/// The Android row for an agent result. `on_android` is `cfg!(target_os = "android")` at the call.
/// `adapter` is [`AccessKitBridge::adapter_name`]; `adapter_error` is [`AccessKitBridge::adapter_error`].
pub fn android_bridge_status(
    on_android: bool,
    adapter: Option<&str>,
    adapter_error: Option<&str>,
) -> AndroidBridgeStatus {
    if !on_android {
        return AndroidBridgeStatus {
            os: false,
            registered: false,
            attached: false,
            status: "not_android",
            detail: "this process is not Android",
        };
    }
    let (attached, status, detail) = if adapter == Some("android") {
        (
            true,
            "attached",
            "TalkBack activates the bridge; the next update is a full tree",
        )
    } else if adapter_error.is_some() {
        (
            false,
            "disabled",
            "the Android adapter was not created or was turned off",
        )
    } else {
        (false, "pending", "the window is not created yet")
    };
    AndroidBridgeStatus {
        os: true,
        registered: true,
        attached,
        status,
        detail,
    }
}

/// Counters of one bridge, for the agent and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeStats {
    /// Updates the UI sent.
    pub updates: u64,
    /// Of those, full trees (the first one, and one after every activation or resync).
    pub full_updates: u64,
    /// Nodes in all updates sent.
    pub nodes_sent: u64,
    /// Updates handed to an OS adapter.
    pub delivered: u64,
    /// Updates dropped because a later full tree replaced them, or the queue overflowed.
    pub superseded: u64,
    /// Updates the platform tree check refused (a child or the focus that does not exist).
    pub invalid_updates: u64,
    /// Updates replaced by the whole platform tree at delivery, because assistive technology connected
    /// between taking the update and handing it to the adapter (the adapter's thread activates it).
    pub resyncs: u64,
    /// Action requests received (from the adapter or the agent).
    pub actions: u64,
    /// Action requests dropped because the queue was full.
    pub actions_dropped: u64,
    /// Times assistive technology connected.
    pub activations: u64,
    /// Times it went away.
    pub deactivations: u64,
}

/// Why an update does not describe a valid platform tree (the adapters panic on these, so the bridge
/// checks first and counts them in [`BridgeStats::invalid_updates`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformTreeError {
    /// The first update carried no `tree` (root).
    NoTree,
    /// The root node is not in the tree.
    MissingRoot(u64),
    /// A node lists a child that is in neither the update nor the tree.
    MissingChild {
        /// The parent.
        parent: u64,
        /// The missing child.
        child: u64,
    },
    /// A node is the child of two parents.
    TwoParents(u64),
    /// The focus names a node that is not in the tree.
    MissingFocus(u64),
}

impl PlatformTreeError {
    /// The protocol reason (`snake_case`).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NoTree => "no_tree",
            Self::MissingRoot(_) => "missing_root",
            Self::MissingChild { .. } => "missing_child",
            Self::TwoParents(_) => "two_parents",
            Self::MissingFocus(_) => "missing_focus",
        }
    }
}

impl std::fmt::Display for PlatformTreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTree => write!(f, "the first update has no tree root"),
            Self::MissingRoot(id) => write!(f, "root #{id} is not in the tree"),
            Self::MissingChild { parent, child } => write!(f, "#{parent} lists missing child #{child}"),
            Self::TwoParents(id) => write!(f, "#{id} has two parents"),
            Self::MissingFocus(id) => write!(f, "focus #{id} is not in the tree"),
        }
    }
}

/// The tree an adapter holds: the result of applying every update in order (nodes replace their
/// old version; nodes no parent reaches any more are dropped).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlatformTree {
    /// The root, once a full update arrived.
    pub root: Option<NodeId>,
    /// The focused node (the root when nothing in the UI has focus).
    pub focus: Option<NodeId>,
    /// The toolkit name of the last full update.
    pub toolkit_name: Option<String>,
    /// The toolkit version of the last full update.
    pub toolkit_version: Option<String>,
    /// Every node.
    pub nodes: HashMap<NodeId, Node>,
}

impl PlatformTree {
    /// Applies `update`. The update is applied even when it is invalid (so the mirror stays close to
    /// what an adapter would hold); the error says what an adapter would have refused.
    pub fn apply(&mut self, update: &TreeUpdate) -> Result<(), PlatformTreeError> {
        if let Some(tree) = &update.tree {
            if self.root != Some(tree.root) {
                self.nodes.clear();
            }
            self.root = Some(tree.root);
            self.toolkit_name.clone_from(&tree.toolkit_name);
            self.toolkit_version.clone_from(&tree.toolkit_version);
        }
        for (id, node) in &update.nodes {
            self.nodes.insert(*id, node.clone());
        }
        self.focus = Some(update.focus);
        let root = self.root.ok_or(PlatformTreeError::NoTree)?;
        if !self.nodes.contains_key(&root) {
            return Err(PlatformTreeError::MissingRoot(root.0));
        }
        // Walk from the root: children must exist and have one parent; unreached nodes go.
        let mut reached: HashSet<NodeId> = HashSet::with_capacity(self.nodes.len());
        reached.insert(root);
        let mut stack = vec![root];
        let mut error = None;
        while let Some(id) = stack.pop() {
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            for child in node.children() {
                if !self.nodes.contains_key(child) {
                    error.get_or_insert(PlatformTreeError::MissingChild {
                        parent: id.0,
                        child: child.0,
                    });
                } else if !reached.insert(*child) {
                    error.get_or_insert(PlatformTreeError::TwoParents(child.0));
                } else {
                    stack.push(*child);
                }
            }
        }
        self.nodes.retain(|id, _| reached.contains(id));
        if let Some(error) = error {
            return Err(error);
        }
        if !self.nodes.contains_key(&update.focus) {
            return Err(PlatformTreeError::MissingFocus(update.focus.0));
        }
        Ok(())
    }

    /// The whole tree as one full update (root first, depth first), or `None` before the first full
    /// update. Only meaningful while the last [`Self::apply`] succeeded.
    pub fn full_update(&self) -> Option<TreeUpdate> {
        let root = self.root?;
        let nodes: Vec<(NodeId, Node)> = self
            .depth_first()
            .into_iter()
            .filter_map(|(id, _)| self.nodes.get(&id).map(|n| (id, n.clone())))
            .collect();
        let focus = self.focus.filter(|f| self.nodes.contains_key(f)).unwrap_or(root);
        Some(TreeUpdate {
            nodes,
            tree: Some(accesskit::Tree {
                root,
                toolkit_name: self.toolkit_name.clone(),
                toolkit_version: self.toolkit_version.clone(),
            }),
            tree_id: accesskit::TreeId::ROOT,
            focus,
        })
    }

    /// The node `id`.
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    /// Every reachable node, depth first from the root, with its parent.
    pub fn depth_first(&self) -> Vec<(NodeId, Option<NodeId>)> {
        let Some(root) = self.root else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut stack = vec![(root, None)];
        let mut seen = HashSet::new();
        while let Some((id, parent)) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            out.push((id, parent));
            if let Some(node) = self.nodes.get(&id) {
                stack.extend(node.children().iter().rev().map(|c| (*c, Some(id))));
            }
        }
        out
    }

    /// The actions `node` supports, in [`ACTION_NAMES`] order.
    pub fn actions(node: &Node) -> Vec<Action> {
        ACTION_NAMES
            .iter()
            .map(|(a, _)| *a)
            .filter(|a| node.supports_action(*a))
            .collect()
    }
}

#[derive(Debug, Default)]
struct Shared {
    active: bool,
    needs_full: bool,
    adapter: Option<&'static str>,
    adapter_error: Option<String>,
    outbound: VecDeque<TreeUpdate>,
    inbound: VecDeque<ActionRequest>,
    mirror: PlatformTree,
    /// The last update applied to `mirror` passed the check, so `mirror` is a valid whole tree.
    mirror_ok: bool,
    last_error: Option<PlatformTreeError>,
    stats: BridgeStats,
    /// Wakes the host's event loop when an adapter thread changes something (ADR-7746).
    waker: Option<BridgeWaker>,
}

/// The host's wake call ([`AccessKitBridge::set_waker`]).
#[derive(Clone)]
struct BridgeWaker(Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for BridgeWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BridgeWaker")
    }
}

/// The two ends of the screen-reader bridge: tree updates from the UI to the OS adapter, action
/// requests from the adapter to the UI, and the activation state. Cheap to clone (one `Arc`); a
/// resource while the `accesskit` feature of `regia_ui` is on.
#[derive(Resource, Debug, Clone, Default)]
pub struct AccessKitBridge {
    shared: Arc<Mutex<Shared>>,
}

impl AccessKitBridge {
    /// A bridge with no adapter, inactive.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        // A poisoned lock only means a panic elsewhere while holding it; the state stays usable.
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether assistive technology is connected (or a test or agent said so).
    pub fn is_active(&self) -> bool {
        self.lock().active
    }

    /// What an adapter does when assistive technology connects: active, and the next update must be
    /// a full tree.
    pub fn activate(&self) {
        let waker = {
            let mut s = self.lock();
            s.active = true;
            s.needs_full = true;
            s.stats.activations += 1;
            s.waker.clone()
        };
        wake(waker);
    }

    /// What an adapter does when assistive technology goes away.
    pub fn deactivate(&self) {
        let waker = {
            let mut s = self.lock();
            if s.active {
                s.stats.deactivations += 1;
            }
            s.active = false;
            s.waker.clone()
        };
        wake(waker);
    }

    /// Sets the call that wakes the host's event loop whenever an adapter thread activates or
    /// deactivates the bridge or queues an action request: a host that sleeps between frames (the
    /// editor's 1 Hz idle) then draws the next frame at once instead of after its idle timeout
    /// (ADR-7746). The call runs on the adapter's thread, outside the bridge's lock.
    pub fn set_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        self.lock().waker = Some(BridgeWaker(Arc::new(waker)));
    }

    /// The next update must be a full tree (first update, after an activation or a resync).
    pub fn needs_full_tree(&self) -> bool {
        let s = self.lock();
        s.needs_full || s.mirror.root.is_none()
    }

    /// Marks an adapter as attached: from now on updates queue for it. `name` is reported by the
    /// agent (`atspi`, `uia`, `ns_accessibility`, `android`, `recording` in tests).
    pub fn attach_adapter(&self, name: &'static str) {
        let mut s = self.lock();
        s.adapter = Some(name);
        s.adapter_error = None;
    }

    /// Records why no OS adapter could be created (reported by the agent).
    pub fn set_adapter_error(&self, error: impl Into<String>) {
        self.lock().adapter_error = Some(error.into());
    }

    /// Detaches the adapter after a JNI failure: later updates are not queued, a queued backlog is
    /// dropped, and `error` is what the agent reads. The window's adapter object may still exist; it
    /// no-ops once this has run.
    pub fn disable_adapter(&self, error: impl Into<String>) {
        let mut s = self.lock();
        s.adapter = None;
        s.adapter_error = Some(error.into());
        let dropped = s.outbound.len() as u64;
        s.stats.superseded += dropped;
        s.outbound.clear();
    }

    /// The attached adapter's name.
    pub fn adapter_name(&self) -> Option<&'static str> {
        self.lock().adapter
    }

    /// Why no OS adapter was created, when one was tried and failed.
    pub fn adapter_error(&self) -> Option<String> {
        self.lock().adapter_error.clone()
    }

    /// Sends `update` from the UI: applied to the platform tree, then queued for the adapter (when
    /// one is attached). A full update (`tree` set) clears the full-tree request and replaces every
    /// queued update. The error is what an adapter would have refused; the update is counted invalid
    /// and not queued, and the next update is a full tree.
    pub fn push_update(&self, update: TreeUpdate) -> Result<(), PlatformTreeError> {
        let mut s = self.lock();
        let full = update.tree.is_some();
        s.stats.updates += 1;
        s.stats.nodes_sent += update.nodes.len() as u64;
        if full {
            s.stats.full_updates += 1;
            s.needs_full = false;
            let dropped = s.outbound.len() as u64;
            s.stats.superseded += dropped;
            s.outbound.clear();
        }
        let applied = s.mirror.apply(&update);
        s.mirror_ok = applied.is_ok();
        if let Err(error) = applied {
            s.stats.invalid_updates += 1;
            s.last_error = Some(error.clone());
            s.needs_full = true;
            return Err(error);
        }
        if s.adapter.is_some() {
            if s.outbound.len() >= MAX_QUEUED_UPDATES {
                // The adapter is not being driven: drop the backlog and resync with a full tree.
                let dropped = s.outbound.len() as u64 + 1;
                s.stats.superseded += dropped;
                s.outbound.clear();
                s.needs_full = true;
            } else {
                s.outbound.push_back(update);
            }
        }
        Ok(())
    }

    /// The updates for the adapter, oldest first. While a full tree is still owed (an activation
    /// came after the last full update), nothing is handed out: an adapter that just activated must
    /// get a full tree first.
    pub fn take_updates(&self) -> Vec<TreeUpdate> {
        let mut s = self.lock();
        if s.needs_full {
            let dropped = s.outbound.len() as u64;
            s.stats.superseded += dropped;
            s.outbound.clear();
            return Vec::new();
        }
        let updates: Vec<TreeUpdate> = s.outbound.drain(..).collect();
        s.stats.delivered += updates.len() as u64;
        updates
    }

    /// What the adapter is handed for `update`, decided at the moment the adapter applies it (the
    /// adapter calls this inside its update, under its own lock): normally `update` itself; but when
    /// assistive technology connected after the update was taken, the adapter now waits for a full
    /// tree. A partial update is replaced with the whole platform tree; a full-tree update meets the
    /// request as it is. Either way the full-tree request clears, so the next `take_updates` hands
    /// partials out again.
    pub fn for_adapter(&self, update: TreeUpdate) -> TreeUpdate {
        let mut s = self.lock();
        if s.needs_full && s.mirror_ok {
            if update.tree.is_some() {
                // The update is a full tree: the adapter applying it meets the full-tree request
                // (an activation or resync that arrived between take and apply), so the next
                // `take_updates` must not hold partials back.
                s.needs_full = false;
            } else if let Some(full) = s.mirror.full_update() {
                s.needs_full = false;
                s.stats.resyncs += 1;
                return full;
            }
        }
        update
    }

    /// The next update the adapter is handed is the whole platform tree ([`Self::for_adapter`]),
    /// without counting an activation: for an adapter that skipped updates and so holds a stale
    /// tree (the Android adapter while accessibility was off, ADR-4932).
    pub fn request_resync(&self) {
        self.lock().needs_full = true;
    }

    /// Queues an action request for the UI (what an adapter does when assistive technology asks a
    /// node to act; the agent's `ui.a11y_platform_action` does the same).
    pub fn push_action(&self, request: ActionRequest) {
        let waker = {
            let mut s = self.lock();
            s.stats.actions += 1;
            if s.inbound.len() >= MAX_QUEUED_ACTIONS {
                s.inbound.pop_front();
                s.stats.actions_dropped += 1;
            }
            s.inbound.push_back(request);
            s.waker.clone()
        };
        wake(waker);
    }

    /// Takes every queued action request, oldest first.
    pub fn take_actions(&self) -> Vec<ActionRequest> {
        self.lock().inbound.drain(..).collect()
    }

    /// Whether action requests are waiting.
    pub fn has_actions(&self) -> bool {
        !self.lock().inbound.is_empty()
    }

    /// A copy of the platform tree (the tree as the adapter has it).
    pub fn platform_tree(&self) -> PlatformTree {
        self.lock().mirror.clone()
    }

    /// Runs `f` on the platform tree without copying it.
    pub fn with_platform_tree<R>(&self, f: impl FnOnce(&PlatformTree) -> R) -> R {
        f(&self.lock().mirror)
    }

    /// The last refused update's error.
    pub fn last_error(&self) -> Option<PlatformTreeError> {
        self.lock().last_error.clone()
    }

    /// The counters.
    pub fn stats(&self) -> BridgeStats {
        self.lock().stats
    }

    /// Forgets the platform tree and queued updates (the UI stopped exporting): the next update is a
    /// full tree.
    pub fn reset_tree(&self) {
        let mut s = self.lock();
        s.mirror = PlatformTree::default();
        let dropped = s.outbound.len() as u64;
        s.stats.superseded += dropped;
        s.outbound.clear();
        s.needs_full = true;
    }
}

/// Runs the host's wake call, if one is set ([`AccessKitBridge::set_waker`]).
fn wake(waker: Option<BridgeWaker>) {
    if let Some(BridgeWaker(call)) = waker {
        call();
    }
}

/// The OS adapter of a window a host creates and drives itself, outside the runner: the editor's
/// window (ADR-7746). The same adapter the runner attaches to the primary window (AT-SPI, UI
/// Automation, NSAccessibility); not `Send`, so it lives on the window's thread.
///
/// On Windows and macOS the adapter subclasses the window before it is first shown: create the
/// window hidden when [`WindowAdapter::must_create_hidden`] says so, attach, then show it.
pub struct WindowAdapter {
    adapter: Box<dyn OsAccessKitAdapter>,
    focused: Option<bool>,
}

impl std::fmt::Debug for WindowAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowAdapter")
            .field("name", &Self::name())
            .field("focused", &self.focused)
            .finish_non_exhaustive()
    }
}

impl WindowAdapter {
    /// The adapter's name in reports (`atspi`, `uia`, `ns_accessibility`, `android`, `none`).
    pub fn name() -> &'static str {
        crate::platform::accesskit_adapter::NAME
    }

    /// Whether the host must create its window hidden and show it after [`Self::attach`]:
    /// the Windows and macOS adapters, unless `REGIA_ACCESSKIT=0` keeps the adapter off.
    pub fn must_create_hidden() -> bool {
        crate::platform::accesskit_adapter::CREATE_HIDDEN && adapter_enabled()
    }

    /// Creates the OS adapter for `window` over `bridge` and marks it attached in the bridge. Refused
    /// (the reason also lands in [`AccessKitBridge::adapter_error`]) when `REGIA_ACCESSKIT=0`, on an
    /// OS without an adapter, or when the window was already shown on Windows.
    pub fn attach(window: &winit::window::Window, bridge: &AccessKitBridge) -> Result<Self, String> {
        if !adapter_enabled() {
            bridge.set_adapter_error("REGIA_ACCESSKIT=0");
            return Err("REGIA_ACCESSKIT=0".to_owned());
        }
        match crate::platform::accesskit_adapter::create(window, bridge) {
            Ok(adapter) => {
                bridge.attach_adapter(Self::name());
                Ok(Self {
                    adapter,
                    focused: None,
                })
            }
            Err(error) => {
                bridge.set_adapter_error(error.clone());
                Err(error)
            }
        }
    }

    /// Hands the bridge's queued tree updates and the window's focus state to the OS adapter (what
    /// `drive_accesskit_adapter` does for the runner's window). Call once a frame, on the window's
    /// thread, after the frame's update was pushed.
    pub fn drive(&mut self, bridge: &AccessKitBridge, window_focused: bool) {
        let _span = tracing::trace_span!("editor_a11y_adapter").entered();
        if self.focused != Some(window_focused) {
            self.focused = Some(window_focused);
            self.adapter.set_window_focused(window_focused);
        }
        for update in bridge.take_updates() {
            self.adapter.update(Box::new(|| bridge.for_adapter(update)));
        }
    }
}

/// The handlers an OS adapter calls (from its own thread): activation, action and deactivation all
/// land in the [`AccessKitBridge`].
#[derive(Debug, Clone)]
pub struct BridgeHandler {
    bridge: AccessKitBridge,
}

impl BridgeHandler {
    /// Handlers over `bridge`.
    pub fn new(bridge: AccessKitBridge) -> Self {
        Self { bridge }
    }
}

impl accesskit::ActivationHandler for BridgeHandler {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        // The tree is built on the main thread with the world; the adapter waits for the next frame's
        // full update (`update_if_active`).
        self.bridge.activate();
        None
    }
}

impl accesskit::ActionHandler for BridgeHandler {
    fn do_action(&mut self, request: ActionRequest) {
        self.bridge.push_action(request);
    }
}

impl accesskit::DeactivationHandler for BridgeHandler {
    fn deactivate_accessibility(&mut self) {
        self.bridge.deactivate();
    }
}

/// One OS adapter (`platform/<os>.rs`), driven on the main thread.
pub(crate) trait OsAccessKitAdapter {
    /// Applies the update `factory` makes, only while assistive technology is connected (the adapter
    /// calls it under its own lock, so the factory sees the activation state the adapter acts on).
    fn update(&mut self, factory: Box<dyn FnOnce() -> TreeUpdate + '_>);
    /// The window gained or lost focus.
    fn set_window_focused(&mut self, focused: bool);
}

/// The adapter of the primary window (not `Send`: the Windows and macOS adapters live on the window's
/// thread). The runner's half: `accesskit` only, not `accesskit-adapters` (ADR-7795).
#[cfg(feature = "accesskit")]
pub(crate) struct AdapterSlot {
    adapter: Box<dyn OsAccessKitAdapter>,
    window: Entity,
    focused: Option<bool>,
}

/// Whether the runner creates the window hidden and shows it after the adapter is attached (the
/// Windows adapter must subclass the window before it is first shown).
#[cfg(feature = "accesskit")]
pub(crate) fn wants_hidden_window(world: &World) -> bool {
    crate::platform::accesskit_adapter::CREATE_HIDDEN
        && adapter_enabled()
        && world.contains_resource::<AccessKitBridge>()
}

fn adapter_enabled() -> bool {
    !matches!(regia_core::brand::env_var("REGIA_ACCESSKIT").as_deref(), Ok("0"))
}

/// Creates the OS adapter for `window` (entity `entity`) when a bridge exists; called by the runner
/// right after it creates the primary window.
#[cfg(feature = "accesskit")]
pub(crate) fn attach_to_window(world: &mut World, entity: Entity, window: &winit::window::Window) {
    let Some(bridge) = world.get_resource::<AccessKitBridge>().cloned() else {
        return;
    };
    if !adapter_enabled() {
        bridge.set_adapter_error("REGIA_ACCESSKIT=0");
        tracing::info!(target: "regia_platform", "screen-reader bridge: OS adapter off (REGIA_ACCESSKIT=0)");
        return;
    }
    match crate::platform::accesskit_adapter::create(window, &bridge) {
        Ok(adapter) => {
            bridge.attach_adapter(crate::platform::accesskit_adapter::NAME);
            tracing::info!(
                target: "regia_platform",
                "screen-reader bridge: {} adapter attached",
                crate::platform::accesskit_adapter::NAME
            );
            world.insert_non_send(AdapterSlot {
                adapter,
                window: entity,
                focused: None,
            });
        }
        Err(error) => {
            tracing::warn!(target: "regia_platform", "screen-reader bridge: no OS adapter: {error}");
            bridge.set_adapter_error(error);
        }
    }
}

/// `Last`: hands the queued tree updates and the window's focus state to the OS adapter. Does
/// nothing without a bridge or an adapter. `accesskit` only: `accesskit-adapters` adds no system
/// (ADR-7795).
#[cfg(feature = "accesskit")]
pub fn drive_accesskit_adapter(world: &mut World) {
    let Some(bridge) = world.get_resource::<AccessKitBridge>().cloned() else {
        return;
    };
    let focused_window = world.get_resource::<crate::FocusedWindow>().and_then(|f| f.0);
    let Some(mut slot) = world.get_non_send_mut::<AdapterSlot>() else {
        return;
    };
    let _span = tracing::trace_span!("ui_a11y_adapter").entered();
    let focused = focused_window == Some(slot.window);
    if slot.focused != Some(focused) {
        slot.focused = Some(focused);
        slot.adapter.set_window_focused(focused);
    }
    for update in bridge.take_updates() {
        slot.adapter.update(Box::new(|| bridge.for_adapter(update)));
    }
}

/// What the Android adapter does with an update ([`android_update_gate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AndroidUpdateGate {
    /// Accessibility is on: hand the update to the adapter.
    Send,
    /// Accessibility is off: drop the update. Android refuses accessibility events then, and
    /// `accesskit_android` 0.7 unwraps that refusal on the UI thread, which aborts the process.
    Hold,
    /// Accessibility is back on after held updates: hand the adapter the whole tree
    /// ([`AccessKitBridge::request_resync`]), since its own tree is stale.
    Resync,
}

/// The Android adapter's gate for one update (ADR-4932). `accessibility_on` is what
/// `AccessibilityManager.isEnabled()` says now. `held` records that an update was dropped while it
/// was off; the first update after it is back on resyncs.
pub fn android_update_gate(accessibility_on: bool, held: &mut bool) -> AndroidUpdateGate {
    match (accessibility_on, *held) {
        (false, _) => {
            *held = true;
            AndroidUpdateGate::Hold
        }
        (true, true) => {
            *held = false;
            AndroidUpdateGate::Resync
        }
        (true, false) => AndroidUpdateGate::Send,
    }
}

/// `update` with each node's description folded into its label, for the Android adapter
/// (ADR-4932). `accesskit_android` 0.7 passes only the label to
/// `AccessibilityNodeInfo.setContentDescription` and drops the description, so TalkBack would read
/// the rebind screen's four toggles as "Invert Y" with no row or device. A node with label
/// `Invert Y` and description `Move, gamepad` leaves as label `Invert Y, Move, gamepad` with no
/// description. A node with only a description gets it as its label. A blank description, or one
/// equal to the label, is dropped. Roles, actions, children, the tree and the focus are unchanged.
/// Desktop adapters, which read both fields, and the agent's [`PlatformTree`] never see the result.
#[must_use]
pub fn fold_descriptions_into_labels(mut update: TreeUpdate) -> TreeUpdate {
    for (_, node) in &mut update.nodes {
        let Some(description) = node.description().map(|d| d.trim().to_owned()) else {
            continue;
        };
        node.clear_description();
        if description.is_empty() {
            continue;
        }
        let folded = match node.label() {
            Some(label) if label.trim() == description => continue,
            Some(label) if !label.trim().is_empty() => format!("{}, {description}", label.trim()),
            _ => description,
        };
        node.set_label(folded);
    }
    update
}

#[cfg(test)]
mod tests {
    use accesskit::{Node, Role, Tree, TreeId};

    use super::*;

    fn node(children: &[u64]) -> Node {
        let mut n = Node::new(Role::Group);
        n.set_children(children.iter().map(|c| NodeId(*c)).collect::<Vec<_>>());
        n
    }

    fn update(nodes: Vec<(u64, Node)>, root: Option<u64>, focus: u64) -> TreeUpdate {
        TreeUpdate {
            nodes: nodes.into_iter().map(|(id, n)| (NodeId(id), n)).collect(),
            tree: root.map(|r| Tree::new(NodeId(r))),
            tree_id: TreeId::ROOT,
            focus: NodeId(focus),
        }
    }

    /// ADR-4932, from the TEST Pixel 9 Pro: an update sent after Android's accessibility turned
    /// off (TalkBack or another client went away) threw in `accesskit_android`'s UI-thread
    /// callback and aborted the game. Updates hold while it is off; the first one after it is
    /// back on carries the whole tree.
    #[test]
    fn android_updates_hold_while_accessibility_is_off_and_resync_after() {
        use AndroidUpdateGate::{Hold, Resync, Send};
        let mut held = false;
        assert_eq!(android_update_gate(true, &mut held), Send);
        assert_eq!(android_update_gate(false, &mut held), Hold);
        assert_eq!(android_update_gate(false, &mut held), Hold);
        assert_eq!(android_update_gate(true, &mut held), Resync);
        assert_eq!(android_update_gate(true, &mut held), Send);

        // A resync hands the adapter the whole tree in place of the next small update.
        let bridge = AccessKitBridge::new();
        bridge.attach_adapter("recording");
        bridge.activate();
        bridge
            .push_update(update(vec![(1, node(&[2])), (2, node(&[]))], Some(1), 2))
            .expect("full tree");
        assert_eq!(bridge.take_updates().len(), 1);
        let small = update(vec![(2, node(&[]))], None, 2);
        bridge.push_update(small.clone()).expect("small update");
        let taken = bridge.take_updates();
        assert_eq!(taken.len(), 1);
        bridge.request_resync();
        let handed = bridge.for_adapter(taken[0].clone());
        assert!(handed.tree.is_some(), "a resync sends the whole tree");
        assert_eq!(handed.nodes.len(), 2);
        // Met: the next small update goes as it is.
        assert!(bridge.for_adapter(small).tree.is_none());
    }

    /// Android calls [`AccessKitBridge::request_resync`] after [`AccessKitBridge::take_updates`]
    /// has already handed the update over. TalkBack coming back on is answered with a full tree
    /// (an activation). That full tree meets the resync: `needs_full` must clear, or the next
    /// `take_updates` drops every partial and the screen reader freezes on the first tree.
    #[test]
    fn a_full_tree_meets_a_resync_requested_after_it_was_taken() {
        let bridge = AccessKitBridge::new();
        bridge.attach_adapter("android");
        bridge
            .push_update(update(vec![(1, node(&[2])), (2, node(&[]))], Some(1), 2))
            .expect("full tree");
        let taken = bridge.take_updates();
        assert_eq!(taken.len(), 1);
        assert!(taken[0].tree.is_some());
        assert!(!bridge.needs_full_tree());

        // The adapter's gate: accessibility is back, so it asks for a resync of this update.
        bridge.request_resync();
        assert!(bridge.needs_full_tree());
        let handed = bridge.for_adapter(taken[0].clone());
        assert!(handed.tree.is_some(), "the full tree is what the adapter applies");
        assert!(
            !bridge.needs_full_tree(),
            "a full tree handed to the adapter meets the resync"
        );

        bridge
            .push_update(update(vec![(2, node(&[]))], None, 2))
            .expect("partial");
        let later = bridge.take_updates();
        assert_eq!(
            later.len(),
            1,
            "partial updates after a full-tree resync must still be delivered, got {}",
            later.len()
        );
        assert!(later[0].tree.is_none(), "the follow-up stays a partial update");
    }

    /// ADR-4932: what TalkBack reads is the label alone, so the Android adapter gets the
    /// description folded into it; nodes without one, and the input's other fields, are untouched.
    #[test]
    fn android_labels_carry_the_description_a_reader_would_otherwise_lose() {
        let labelled = |label: Option<&str>, description: Option<&str>| {
            let mut n = Node::new(Role::CheckBox);
            if let Some(label) = label {
                n.set_label(label);
            }
            if let Some(description) = description {
                n.set_description(description);
            }
            n
        };
        let input = update(
            vec![
                (1, node(&[2, 3, 4, 5, 6, 7])),
                (2, labelled(Some("Invert Y"), Some("Move, gamepad"))),
                (3, labelled(Some("Invert Y"), Some("Look, gamepad"))),
                (4, labelled(None, Some("Move, gamepad"))),
                (5, labelled(Some("Reset"), None)),
                (6, labelled(Some("Same"), Some("Same"))),
                (7, labelled(Some("  Dead zone "), Some("   "))),
            ],
            Some(1),
            2,
        );
        let folded = fold_descriptions_into_labels(input.clone());
        let read = |id: u64| {
            let (_, n) = folded
                .nodes
                .iter()
                .find(|(n, _)| *n == NodeId(id))
                .expect("node kept");
            (n.label().map(str::to_owned), n.description().map(str::to_owned))
        };
        assert_eq!(read(2), (Some("Invert Y, Move, gamepad".into()), None));
        assert_eq!(read(3), (Some("Invert Y, Look, gamepad".into()), None));
        assert_eq!(read(4), (Some("Move, gamepad".into()), None));
        assert_eq!(read(5), (Some("Reset".into()), None));
        assert_eq!(read(6), (Some("Same".into()), None));
        // A blank description adds nothing; the label is passed on as the UI wrote it.
        assert_eq!(read(7), (Some("  Dead zone ".into()), None));
        // Everything but label and description is the input's.
        assert_eq!(folded.nodes.len(), input.nodes.len());
        assert_eq!((folded.tree, folded.focus), (input.tree, input.focus));
        for ((a, before), (b, after)) in input.nodes.iter().zip(&folded.nodes) {
            assert_eq!(a, b);
            assert_eq!(before.role(), after.role());
            assert_eq!(before.children(), after.children());
        }
    }

    #[test]
    fn the_mirror_drops_unreached_nodes_and_checks_children() {
        let mut tree = PlatformTree::default();
        assert_eq!(
            tree.apply(&update(vec![(1, node(&[]))], None, 1)),
            Err(PlatformTreeError::NoTree)
        );
        tree.apply(&update(
            vec![(1, node(&[2, 3])), (2, node(&[])), (3, node(&[]))],
            Some(1),
            1,
        ))
        .expect("valid");
        assert_eq!(tree.nodes.len(), 3);
        // 3 leaves the root's children: it is dropped.
        tree.apply(&update(vec![(1, node(&[2]))], None, 2))
            .expect("valid");
        assert_eq!(tree.nodes.len(), 2);
        assert_eq!(tree.focus, Some(NodeId(2)));
        assert_eq!(
            tree.apply(&update(vec![(1, node(&[2, 9]))], None, 1)),
            Err(PlatformTreeError::MissingChild { parent: 1, child: 9 })
        );
        assert_eq!(
            tree.apply(&update(vec![(1, node(&[2]))], None, 7)),
            Err(PlatformTreeError::MissingFocus(7))
        );
        assert_eq!(
            tree.depth_first(),
            vec![(NodeId(1), None), (NodeId(2), Some(NodeId(1)))]
        );
    }

    #[test]
    fn an_activation_holds_updates_until_a_full_tree() {
        let bridge = AccessKitBridge::new();
        bridge.attach_adapter("recording");
        assert!(bridge.needs_full_tree());
        bridge
            .push_update(update(vec![(1, node(&[]))], Some(1), 1))
            .expect("valid");
        assert!(!bridge.needs_full_tree());
        bridge
            .push_update(update(vec![(1, node(&[]))], None, 1))
            .expect("valid");
        assert_eq!(bridge.take_updates().len(), 2);
        // Assistive technology connects: incremental updates already queued are dropped, nothing is
        // handed out until the full tree arrives.
        bridge
            .push_update(update(vec![(1, node(&[]))], None, 1))
            .expect("valid");
        bridge.activate();
        assert!(bridge.is_active() && bridge.needs_full_tree());
        assert!(bridge.take_updates().is_empty());
        bridge
            .push_update(update(vec![(1, node(&[]))], Some(1), 1))
            .expect("valid");
        let handed = bridge.take_updates();
        assert_eq!(handed.len(), 1);
        assert!(handed[0].tree.is_some());
        let stats = bridge.stats();
        assert_eq!((stats.updates, stats.full_updates, stats.delivered), (4, 2, 3));
        assert_eq!(stats.superseded, 1);
    }

    #[test]
    fn an_activation_between_take_and_apply_gets_the_whole_tree() {
        let bridge = AccessKitBridge::new();
        bridge.attach_adapter("recording");
        bridge
            .push_update(update(vec![(1, node(&[2])), (2, node(&[]))], Some(1), 2))
            .expect("valid");
        bridge
            .push_update(update(vec![(2, node(&[]))], None, 1))
            .expect("valid");
        let taken = bridge.take_updates();
        assert_eq!(taken.len(), 2);
        // The first goes through as it is; then the adapter's thread activates before the second.
        let mut handed: Vec<TreeUpdate> = Vec::new();
        let mut taken = taken.into_iter();
        handed.push(bridge.for_adapter(taken.next().expect("first")));
        bridge.activate();
        handed.push(bridge.for_adapter(taken.next().expect("second")));
        assert!(handed[0].tree.is_some());
        let resync = &handed[1];
        assert!(resync.tree.is_some(), "a pending adapter must get a full tree");
        assert_eq!(resync.nodes.len(), 2);
        assert_eq!(resync.nodes[0].0, NodeId(1));
        assert_eq!(resync.focus, NodeId(1));
        // The request is met: nothing more is held back, and the UI need not send another full tree.
        assert!(!bridge.needs_full_tree());
        assert_eq!(bridge.stats().resyncs, 1);
        let mut check = PlatformTree::default();
        check.apply(resync).expect("the resync is a valid whole tree");
    }

    #[test]
    fn an_undriven_adapter_resyncs_instead_of_growing() {
        let bridge = AccessKitBridge::new();
        bridge.attach_adapter("recording");
        bridge
            .push_update(update(vec![(1, node(&[]))], Some(1), 1))
            .expect("valid");
        for _ in 0..MAX_QUEUED_UPDATES {
            bridge
                .push_update(update(vec![(1, node(&[]))], None, 1))
                .expect("valid");
        }
        assert!(bridge.needs_full_tree());
        assert!(bridge.take_updates().is_empty());
        assert_eq!(bridge.stats().superseded, MAX_QUEUED_UPDATES as u64 + 1);
    }

    #[test]
    fn without_an_adapter_nothing_queues() {
        let bridge = AccessKitBridge::new();
        for _ in 0..(MAX_QUEUED_UPDATES * 2) {
            bridge
                .push_update(update(vec![(1, node(&[]))], Some(1), 1))
                .expect("valid");
        }
        assert!(bridge.take_updates().is_empty());
        assert_eq!(bridge.stats().superseded, 0);
        assert_eq!(bridge.platform_tree().nodes.len(), 1);
    }

    #[test]
    fn actions_queue_in_order_and_drop_the_oldest() {
        let bridge = AccessKitBridge::new();
        for i in 0..(MAX_QUEUED_ACTIONS as u64 + 2) {
            bridge.push_action(ActionRequest {
                action: Action::Click,
                target_tree: TreeId::ROOT,
                target_node: NodeId(i),
                data: None,
            });
        }
        let taken = bridge.take_actions();
        assert_eq!(taken.len(), MAX_QUEUED_ACTIONS);
        assert_eq!(taken[0].target_node, NodeId(2));
        assert_eq!(bridge.stats().actions_dropped, 2);
        assert!(!bridge.has_actions());
    }

    #[test]
    fn action_names_round_trip() {
        for (action, name) in ACTION_NAMES {
            assert_eq!(parse_action(name), Some(action));
            assert_eq!(action_name(action), name);
        }
        assert_eq!(parse_action("press"), None);
    }

    #[test]
    fn the_handler_records_activation_actions_and_deactivation() {
        use accesskit::{ActionHandler, ActivationHandler, DeactivationHandler};
        let bridge = AccessKitBridge::new();
        let mut handler = BridgeHandler::new(bridge.clone());
        assert!(handler.request_initial_tree().is_none());
        assert!(bridge.is_active());
        handler.do_action(ActionRequest {
            action: Action::Focus,
            target_tree: TreeId::ROOT,
            target_node: NodeId(4),
            data: None,
        });
        assert_eq!(bridge.take_actions().len(), 1);
        handler.deactivate_accessibility();
        assert!(!bridge.is_active());
        let stats = bridge.stats();
        assert_eq!((stats.activations, stats.deactivations, stats.actions), (1, 1, 1));
    }

    #[test]
    fn android_attach_decision_names_every_block() {
        assert!(android_attach_decision(true, true, false).is_ok());
        assert_eq!(
            android_attach_decision(false, true, false),
            Err(AndroidAttachBlock::NoActivity)
        );
        assert_eq!(
            android_attach_decision(false, false, true),
            Err(AndroidAttachBlock::NoActivity)
        );
        assert_eq!(
            android_attach_decision(true, false, false),
            Err(AndroidAttachBlock::NoSurfaceView)
        );
        assert_eq!(
            android_attach_decision(true, true, true),
            Err(AndroidAttachBlock::DelegateAlreadySet)
        );
        assert!(
            AndroidAttachBlock::DelegateAlreadySet
                .reason()
                .contains("accessibility delegate")
        );
    }

    #[test]
    fn disable_adapter_stops_the_queue() {
        let bridge = AccessKitBridge::new();
        bridge.attach_adapter("android");
        bridge
            .push_update(update(vec![(1, node(&[]))], Some(1), 1))
            .expect("valid");
        assert_eq!(bridge.adapter_name(), Some("android"));
        assert_eq!(bridge.take_updates().len(), 1);
        bridge
            .push_update(update(vec![(1, node(&[]))], None, 1))
            .expect("valid");
        bridge.disable_adapter("jni failed");
        assert!(bridge.adapter_name().is_none());
        assert_eq!(bridge.adapter_error().as_deref(), Some("jni failed"));
        assert!(bridge.take_updates().is_empty());
        assert!(bridge.stats().superseded >= 1);
        bridge
            .push_update(update(vec![(1, node(&[]))], None, 1))
            .expect("valid");
        assert!(bridge.take_updates().is_empty());
    }

    #[test]
    fn android_bridge_status_covers_every_state() {
        let off = android_bridge_status(false, Some("recording"), None);
        assert_eq!(off.status, "not_android");
        assert!(!off.os && !off.registered && !off.attached);

        let pending = android_bridge_status(true, None, None);
        assert_eq!(pending.status, "pending");
        assert!(pending.os && pending.registered && !pending.attached);

        let attached = android_bridge_status(true, Some("android"), None);
        assert_eq!(attached.status, "attached");
        assert!(attached.attached);
        assert!(attached.detail.contains("TalkBack"));

        let disabled = android_bridge_status(true, None, Some("REGIA_ACCESSKIT=0"));
        assert_eq!(disabled.status, "disabled");
        assert!(disabled.registered && !disabled.attached);

        let other = android_bridge_status(true, Some("recording"), None);
        assert_eq!(other.status, "pending");
        assert!(!other.attached);
    }

    /// ADR-7746: the editor's event loop sleeps up to a second between idle frames, so an action
    /// request or an activation from an adapter thread wakes it through the bridge's waker.
    #[test]
    fn accesskit_waker_runs_on_actions_and_activation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let bridge = AccessKitBridge::new();
        // No waker yet: nothing to call.
        bridge.activate();
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = woken.clone();
        bridge.set_waker(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        bridge.push_action(ActionRequest {
            action: Action::Click,
            target_tree: TreeId::ROOT,
            target_node: NodeId(7),
            data: None,
        });
        assert_eq!(woken.load(Ordering::SeqCst), 1, "an action wakes the host");
        bridge.deactivate();
        bridge.activate();
        assert_eq!(
            woken.load(Ordering::SeqCst),
            3,
            "deactivation and activation wake it"
        );
        // Updates come from the host's own thread and never wake it.
        let _ = bridge.push_update(update(vec![(1, node(&[]))], Some(1), 1));
        assert_eq!(woken.load(Ordering::SeqCst), 3);
        assert_eq!(bridge.take_actions().len(), 1);
    }
}
