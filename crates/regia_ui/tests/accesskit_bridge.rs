// SPDX-License-Identifier: MPL-2.0
//! The screen-reader OS bridge (ADR-1998) against a recording adapter: every update the UI sends is
//! fed to `accesskit_consumer::Tree`, the tree AccessKit's platform adapters keep (it panics on an
//! update an adapter would refuse), so no screen reader is needed. Headless, no GPU, no clock: frames
//! are plain `app.update()` calls.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use accesskit::{Action, ActionData, ActionRequest, Live, NodeId, Role, Toggled, TreeId, TreeUpdate};
use regia_app::prelude::*;
use regia_platform::accessibility::{AccessKitBridge, PlatformTree};
use regia_ui::a11y::{A11yAction, A11yReject, A11yRole, Accessible, Politeness, ScreenReader};
use regia_ui::accesskit_bridge::{
    ASSERTIVE_ANNOUNCER, AccessKitBackend, CLOSE_ACTION_ID, POLITE_ANNOUNCER, ROOT_NODE,
};
use regia_ui::prelude::*;

/// What a platform adapter does with updates: the first one builds the tree, the rest update it.
#[derive(Default)]
struct RecordingAdapter {
    tree: Option<accesskit_consumer::Tree>,
    updates: Vec<TreeUpdate>,
    changes: Changes,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
struct Changes {
    added: usize,
    updated: usize,
    removed: usize,
    focus_moves: usize,
}

impl accesskit_consumer::TreeChangeHandler for Changes {
    fn node_added(&mut self, _node: &accesskit_consumer::Node) {
        self.added += 1;
    }
    fn node_updated(&mut self, _old: &accesskit_consumer::Node, _new: &accesskit_consumer::Node) {
        self.updated += 1;
    }
    fn focus_moved(
        &mut self,
        _old: Option<&accesskit_consumer::Node>,
        _new: Option<&accesskit_consumer::Node>,
    ) {
        self.focus_moves += 1;
    }
    fn node_removed(&mut self, _node: &accesskit_consumer::Node) {
        self.removed += 1;
    }
}

impl RecordingAdapter {
    /// Takes the bridge's updates, as `drive_accesskit_adapter` does each frame.
    fn drive(&mut self, bridge: &AccessKitBridge) -> usize {
        let updates = bridge.take_updates();
        let n = updates.len();
        for update in updates {
            self.updates.push(update.clone());
            if update.tree.is_some() || self.tree.is_none() {
                assert!(
                    update.tree.is_some(),
                    "the first update an adapter gets must be a full tree"
                );
                self.tree = Some(accesskit_consumer::Tree::new(update, true));
            } else if let Some(tree) = self.tree.as_mut() {
                tree.update_and_process_changes(update, &mut self.changes);
            }
        }
        n
    }

    fn state(&self) -> &accesskit_consumer::TreeState {
        self.tree.as_ref().expect("a tree").state()
    }

    fn focus_label(&self) -> Option<String> {
        self.state()
            .focus()
            .and_then(|n| n.data().label().map(str::to_owned))
    }

    fn last(&self) -> &TreeUpdate {
        self.updates.last().expect("an update")
    }
}

fn headless(budget: usize) -> (App, AccessKitBridge) {
    let bridge = AccessKitBridge::new();
    bridge.attach_adapter("recording");
    let mut app = App::new();
    app.insert_resource(TextSystem::embedded_only());
    app.insert_resource(bridge.clone());
    app.insert_resource(ScreenReader::with_backend(Box::new(
        AccessKitBackend::with_budget(bridge.clone(), budget),
    )));
    app.add_plugins(UiPlugin::code_only());
    with_input_messages(&mut app);
    (app, bridge)
}

/// The window and input messages the UI reads (the platform plugin adds them in a real app).
fn with_input_messages(app: &mut App) {
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
}

struct Controls {
    root: Entity,
    start: Entity,
    /// The label text folded into `start`.
    start_label: Entity,
    toggle: Entity,
    slider: Entity,
    input: Entity,
}

fn controls(world: &mut World) -> Controls {
    let root = world.spawn((UiRoot::default(), UiNode)).id();
    let start = world
        .spawn((UiNode, Button, Focusable::default(), ChildOf(root)))
        .id();
    let start_label = world.spawn((UiNode, Text::new("Start"), ChildOf(start))).id();
    let toggle = world
        .spawn((
            UiNode,
            Toggle { checked: false },
            Focusable::default(),
            ChildOf(root),
        ))
        .id();
    world.spawn((UiNode, Text::new("Subtitles"), ChildOf(toggle)));
    let slider = world
        .spawn((
            UiNode,
            Slider {
                value: 50.0,
                min: 0.0,
                max: 100.0,
                step: 5.0,
            },
            Accessible::default().with_label("Volume"),
            ChildOf(root),
        ))
        .id();
    let mut field = TextInput::default();
    field.placeholder = "Player name".to_owned();
    let input = world.spawn((UiNode, field, ChildOf(root))).id();
    Controls {
        root,
        start,
        start_label,
        toggle,
        slider,
        input,
    }
}

fn node(bridge: &AccessKitBridge, e: Entity) -> accesskit::Node {
    bridge
        .platform_tree()
        .get(NodeId(e.to_bits()))
        .cloned()
        .unwrap_or_else(|| panic!("{e:?} is not in the platform tree"))
}

fn action(bridge: &AccessKitBridge, e: Entity, action: Action, data: Option<ActionData>) {
    bridge.push_action(ActionRequest {
        action,
        target_tree: TreeId::ROOT,
        target_node: NodeId(e.to_bits()),
        data,
    });
}

/// Connects assistive technology and runs one frame: the adapter gets the full tree.
fn connect(app: &mut App, bridge: &AccessKitBridge, adapter: &mut RecordingAdapter) {
    bridge.activate();
    app.update();
    assert_eq!(adapter.drive(bridge), 1);
    assert!(adapter.last().tree.is_some());
}

#[test]
fn nothing_is_built_or_sent_until_assistive_technology_connects() {
    let (mut app, bridge) = headless(256);
    let c = controls(app.world_mut());
    let mut adapter = RecordingAdapter::default();
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(app.world().resource::<ScreenReader>().builds, 0);
    assert_eq!(bridge.stats().updates, 0);
    assert_eq!(adapter.drive(&bridge), 0);

    connect(&mut app, &bridge, &mut adapter);
    let state = adapter.state();
    let root = state.root();
    assert_eq!(bridge.platform_tree().root, Some(NodeId(ROOT_NODE)));
    assert_eq!(root.role(), Role::Window);
    // The UI root plus the two announcers hang off the window.
    assert_eq!(root.data().children().len(), 3);
    let start = node(&bridge, c.start);
    assert_eq!(start.role(), Role::Button);
    assert_eq!(start.label(), Some("Start"));
    assert!(start.supports_action(Action::Click) && start.supports_action(Action::Focus));
    let toggle = node(&bridge, c.toggle);
    assert_eq!(
        (toggle.role(), toggle.toggled()),
        (Role::CheckBox, Some(Toggled::False))
    );
    let slider = node(&bridge, c.slider);
    assert_eq!(
        (
            slider.numeric_value(),
            slider.min_numeric_value(),
            slider.max_numeric_value()
        ),
        (Some(50.0), Some(0.0), Some(100.0))
    );
    assert_eq!(slider.numeric_value_step(), Some(5.0));
    let input = node(&bridge, c.input);
    assert_eq!(input.role(), Role::TextInput);
    assert!(input.supports_action(Action::SetValue));
    // The folded label child is not a node of its own.
    assert!(
        bridge
            .platform_tree()
            .get(NodeId(c.start_label.to_bits()))
            .is_none()
    );
    assert_eq!(bridge.stats().invalid_updates, 0);
}

#[test]
fn ids_are_the_entity_bits_the_ui_tree_reports_and_stay_stable() {
    let (mut app, bridge) = headless(256);
    let c = controls(app.world_mut());
    let mut adapter = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut adapter);
    let ui_ids: Vec<u64> = app
        .world()
        .resource::<ScreenReader>()
        .tree()
        .nodes
        .iter()
        .map(|n| n.id)
        .collect();
    let platform = bridge.platform_tree();
    for id in &ui_ids {
        assert!(
            platform.get(NodeId(*id)).is_some(),
            "ui node {id} missing on the platform"
        );
    }
    // Window root + UI nodes + two announcers.
    assert_eq!(platform.nodes.len(), ui_ids.len() + 3);
    assert!(platform.get(NodeId(c.root.to_bits())).is_some());
    for _ in 0..3 {
        app.update();
        adapter.drive(&bridge);
    }
    assert_eq!(bridge.platform_tree().nodes.keys().len(), platform.nodes.len());
    assert!(bridge.platform_tree().get(NodeId(c.start.to_bits())).is_some());
}

#[test]
fn an_unchanged_frame_sends_nothing_and_a_label_change_sends_one_node() {
    let (mut app, bridge) = headless(256);
    let c = controls(app.world_mut());
    let mut adapter = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut adapter);
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(adapter.drive(&bridge), 0, "a still UI sends no update");
    // An override label: no layout moves, so exactly one node changes.
    app.world_mut().get_mut::<Accessible>(c.slider).unwrap().label = Some("Music volume".to_owned());
    app.update();
    assert_eq!(adapter.drive(&bridge), 1);
    let update = adapter.last();
    assert_eq!(update.nodes.len(), 1);
    assert_eq!(update.nodes[0].0, NodeId(c.slider.to_bits()));
    assert_eq!(update.nodes[0].1.label(), Some("Music volume"));
    assert_eq!(adapter.changes.updated, 1);
}

#[test]
fn focus_moves_reach_the_platform_and_name_the_node() {
    let (mut app, bridge) = headless(256);
    let c = controls(app.world_mut());
    let mut adapter = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut adapter);
    assert_eq!(
        adapter.last().focus,
        NodeId(ROOT_NODE),
        "nothing focused: the window"
    );
    action(&bridge, c.start, Action::Focus, None);
    app.update();
    app.update();
    adapter.drive(&bridge);
    assert_eq!(adapter.last().focus, NodeId(c.start.to_bits()));
    assert_eq!(adapter.focus_label().as_deref(), Some("Start"));
    assert!(adapter.changes.focus_moves >= 1);
}

#[test]
fn added_and_removed_widgets_update_the_parent_and_drop_the_node() {
    let (mut app, bridge) = headless(256);
    let c = controls(app.world_mut());
    let mut adapter = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut adapter);
    let quit = app
        .world_mut()
        .spawn((UiNode, Button, Focusable::default(), ChildOf(c.root)))
        .id();
    app.world_mut().spawn((UiNode, Text::new("Quit"), ChildOf(quit)));
    app.update();
    adapter.drive(&bridge);
    let ids: Vec<NodeId> = adapter.last().nodes.iter().map(|(id, _)| *id).collect();
    assert!(ids.contains(&NodeId(quit.to_bits())) && ids.contains(&NodeId(c.root.to_bits())));
    assert_eq!(node(&bridge, quit).label(), Some("Quit"));
    assert_eq!(adapter.changes.added, 1);
    app.world_mut().entity_mut(quit).despawn();
    app.update();
    adapter.drive(&bridge);
    assert!(bridge.platform_tree().get(NodeId(quit.to_bits())).is_none());
    assert_eq!(adapter.changes.removed, 1);
    assert_eq!(bridge.stats().invalid_updates, 0);
}

#[test]
fn platform_actions_run_through_the_ui_input_path() {
    let (mut app, bridge) = headless(256);
    let c = controls(app.world_mut());
    let mut adapter = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut adapter);
    action(&bridge, c.toggle, Action::Click, None);
    action(&bridge, c.slider, Action::Increment, None);
    action(
        &bridge,
        c.input,
        Action::SetValue,
        Some(ActionData::Value("Ada".into())),
    );
    app.update();
    assert!(app.world().get::<Toggle>(c.toggle).unwrap().checked);
    assert_eq!(app.world().get::<Slider>(c.slider).unwrap().value, 55.0);
    assert_eq!(app.world().get::<TextInput>(c.input).unwrap().text, "Ada");
    app.update();
    adapter.drive(&bridge);
    assert_eq!(node(&bridge, c.toggle).toggled(), Some(Toggled::True));
    assert_eq!(node(&bridge, c.slider).numeric_value(), Some(55.0));
    assert_eq!(node(&bridge, c.input).value(), Some("Ada"));
    // The results are the ones `ui.a11y_act` reports.
    let results: Vec<(A11yAction, Result<(), A11yReject>)> = app
        .world()
        .resource::<ScreenReader>()
        .recent_requests()
        .map(|(r, res)| (r.action.clone(), *res))
        .collect();
    assert_eq!(
        results,
        vec![
            (A11yAction::Press, Ok(())),
            (A11yAction::Raise, Ok(())),
            (A11yAction::SetText("Ada".to_owned()), Ok(())),
        ]
    );
}

#[test]
fn a_disabled_control_offers_nothing_and_refuses_a_request_anyway() {
    let (mut app, bridge) = headless(256);
    let c = controls(app.world_mut());
    app.world_mut().entity_mut(c.toggle).insert(Disabled);
    let mut adapter = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut adapter);
    let toggle = node(&bridge, c.toggle);
    assert!(toggle.is_disabled());
    assert!(PlatformTree::actions(&toggle).is_empty());
    action(&bridge, c.toggle, Action::Click, None);
    app.update();
    assert!(!app.world().get::<Toggle>(c.toggle).unwrap().checked);
    let last = app
        .world()
        .resource::<ScreenReader>()
        .recent_requests()
        .last()
        .cloned();
    assert_eq!(last.map(|(_, r)| r), Some(Err(A11yReject::Disabled)));
}

#[test]
fn the_open_dialog_is_modal_closes_by_its_custom_action_and_the_menu_behind_offers_nothing() {
    let (mut app, bridge) = headless(256);
    let c = controls(app.world_mut());
    let mut adapter = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut adapter);
    let dialog_root = app.world_mut().spawn((UiRoot::default(), UiNode)).id();
    let dialog = app
        .world_mut()
        .spawn((UiNode, Modal::default(), ChildOf(dialog_root)))
        .id();
    let ok = app
        .world_mut()
        .spawn((UiNode, Button, Focusable::default(), ChildOf(dialog)))
        .id();
    app.world_mut().spawn((UiNode, Text::new("OK"), ChildOf(ok)));
    app.update();
    app.update();
    adapter.drive(&bridge);
    let modal = node(&bridge, dialog);
    assert_eq!(modal.role(), Role::Dialog);
    assert!(modal.is_modal());
    assert!(modal.custom_actions().iter().any(|a| a.id == CLOSE_ACTION_ID));
    // The start button sits under the dialog: inert, so no actions.
    assert!(PlatformTree::actions(&node(&bridge, c.start)).is_empty());
    action(
        &bridge,
        dialog,
        Action::CustomAction,
        Some(ActionData::CustomAction(CLOSE_ACTION_ID)),
    );
    app.update();
    assert!(!app.world().get::<Modal>(dialog).unwrap().open);
    app.update();
    app.update();
    adapter.drive(&bridge);
    // A closed dialog is hidden or at least no longer modal.
    assert!(
        bridge
            .platform_tree()
            .get(NodeId(dialog.to_bits()))
            .is_none_or(|n| !n.is_modal())
    );
    assert!(node(&bridge, c.start).supports_action(Action::Click));
    assert_eq!(bridge.stats().invalid_updates, 0);
}

#[test]
fn announcements_change_the_live_region_label() {
    let (mut app, bridge) = headless(256);
    controls(app.world_mut());
    let mut adapter = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut adapter);
    let polite = bridge
        .platform_tree()
        .get(NodeId(POLITE_ANNOUNCER))
        .cloned()
        .unwrap();
    assert_eq!((polite.live(), polite.label()), (Some(Live::Polite), None));
    app.world_mut()
        .resource_mut::<ScreenReader>()
        .announce("Game saved", Politeness::Polite);
    app.world_mut()
        .resource_mut::<ScreenReader>()
        .announce("Low health", Politeness::Assertive);
    app.update();
    adapter.drive(&bridge);
    let tree = bridge.platform_tree();
    assert_eq!(
        tree.get(NodeId(POLITE_ANNOUNCER)).unwrap().label(),
        Some("Game saved")
    );
    let assertive = tree.get(NodeId(ASSERTIVE_ANNOUNCER)).unwrap();
    assert_eq!(
        (assertive.live(), assertive.label()),
        (Some(Live::Assertive), Some("Low health"))
    );
}

#[test]
fn a_large_change_spreads_over_frames_within_the_budget() {
    const LABELS: usize = 600;
    const BUDGET: usize = 100;
    let (mut app, bridge) = headless(BUDGET);
    let root = app.world_mut().spawn((UiRoot::default(), UiNode)).id();
    // Override labels, not text: renaming them moves no layout, so the counts are exact.
    let labels: Vec<Entity> = (0..LABELS)
        .map(|i| {
            app.world_mut()
                .spawn((
                    UiNode,
                    Accessible::default()
                        .with_role(A11yRole::Label)
                        .with_label(format!("row {i}")),
                    ChildOf(root),
                ))
                .id()
        })
        .collect();
    let mut adapter = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut adapter);
    for (i, e) in labels.iter().enumerate() {
        app.world_mut().get_mut::<Accessible>(*e).unwrap().label = Some(format!("line {i}"));
    }
    // One frame takes the change; later frames, with nothing new, drain what waits.
    let mut frames = 0;
    let mut sent = 0;
    loop {
        app.update();
        frames += 1;
        if adapter.drive(&bridge) == 0 {
            break;
        }
        let n = adapter.last().nodes.len();
        assert!(n <= BUDGET, "frame {frames} sent {n} nodes");
        sent += n;
        assert!(frames < 50, "the change never finished");
    }
    assert_eq!(sent, LABELS);
    assert_eq!(frames, LABELS / BUDGET + 1);
    for (i, e) in labels.iter().enumerate() {
        assert_eq!(node(&bridge, *e).label(), Some(format!("line {i}").as_str()));
    }
    assert_eq!(adapter.changes.updated, LABELS);
}

#[test]
fn structural_changes_go_out_even_over_the_budget() {
    let (mut app, bridge) = headless(4);
    let root = app.world_mut().spawn((UiRoot::default(), UiNode)).id();
    let mut adapter = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut adapter);
    for i in 0..20 {
        app.world_mut().spawn((
            UiNode,
            Accessible::default()
                .with_role(A11yRole::Label)
                .with_label(format!("new {i}")),
            ChildOf(root),
        ));
    }
    app.update();
    adapter.drive(&bridge);
    // Twenty new nodes plus their parent, in one consistent update.
    assert_eq!(adapter.last().nodes.len(), 21);
    assert_eq!(bridge.stats().invalid_updates, 0);
}

#[test]
fn a_reconnect_resends_the_whole_tree() {
    let (mut app, bridge) = headless(256);
    let c = controls(app.world_mut());
    let mut adapter = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut adapter);
    let full = adapter.last().nodes.len();
    bridge.deactivate();
    app.world_mut().get_mut::<Toggle>(c.toggle).unwrap().checked = true;
    app.update();
    adapter.drive(&bridge);
    // A new adapter session (the screen reader restarted) starts from a full tree.
    let mut fresh = RecordingAdapter::default();
    connect(&mut app, &bridge, &mut fresh);
    assert_eq!(fresh.last().nodes.len(), full);
    assert_eq!(node(&bridge, c.toggle).toggled(), Some(Toggled::True));
    assert_eq!(bridge.stats().full_updates, 2);
}

/// The calls behind `ui.a11y_platform_connect`, `ui.a11y_platform_tree` and `ui.a11y_platform_action`.
#[test]
fn the_agent_api_connects_dumps_and_acts() {
    use regia_ui::a11y_platform::{
        BRIDGE_BUILT, PlatformActionValue, PlatformBridgeError, platform_tree_json, queue_platform_action,
        set_connected,
    };
    // This test only builds with the feature, so the flag must say the bridge is built in.
    const _: () = assert!(BRIDGE_BUILT);
    let (mut app, bridge) = headless(256);
    let c = controls(app.world_mut());
    app.update();
    let before = platform_tree_json(app.world()).unwrap();
    assert_eq!(
        (before["active"].as_bool(), before["count"].as_u64()),
        (Some(false), Some(0))
    );
    assert_eq!(before["adapter"].as_str(), Some("recording"));
    let android = &before["android"];
    assert_eq!(android["os"].as_bool(), Some(cfg!(target_os = "android")));
    assert_eq!(android["registered"].as_bool(), Some(cfg!(target_os = "android")));
    assert_eq!(android["attached"].as_bool(), Some(false));
    assert!(android["status"].as_str().is_some());
    assert!(android["detail"].as_str().is_some());
    // Nothing is in the platform tree yet, so an action has no target.
    assert_eq!(
        queue_platform_action(app.world(), c.toggle.to_bits(), "click", None),
        Err(PlatformBridgeError::UnknownNode(c.toggle.to_bits()))
    );

    let connected = set_connected(app.world(), true).unwrap();
    assert_eq!(
        (connected["active"].as_bool(), connected["was_active"].as_bool()),
        (Some(true), Some(false))
    );
    assert_eq!(
        connected["android"]["os"].as_bool(),
        Some(cfg!(target_os = "android"))
    );
    assert_eq!(connected["android"]["attached"].as_bool(), Some(false));
    app.update();
    let tree = platform_tree_json(app.world()).unwrap();
    assert_eq!(tree["root"].as_u64(), Some(ROOT_NODE));
    let nodes = tree["nodes"].as_array().unwrap();
    assert_eq!(nodes[0]["role"].as_str(), Some("Window"));
    let toggle = nodes
        .iter()
        .find(|n| n["id"].as_u64() == Some(c.toggle.to_bits()))
        .expect("the check box is in the dump");
    assert_eq!(toggle["role"].as_str(), Some("CheckBox"));
    assert_eq!(toggle["label"].as_str(), Some("Subtitles"));
    assert_eq!(toggle["toggled"].as_str(), Some("false"));
    assert!(toggle["actions"].as_array().unwrap().iter().any(|a| a == "click"));
    let slider = nodes
        .iter()
        .find(|n| n["id"].as_u64() == Some(c.slider.to_bits()))
        .unwrap();
    assert_eq!(
        (slider["numeric_value"].as_f64(), slider["step"].as_f64()),
        (Some(50.0), Some(5.0))
    );
    assert_eq!(tree["stats"]["full_updates"].as_u64(), Some(1));
    assert_eq!(tree["stats"]["invalid_updates"].as_u64(), Some(0));

    // Parameter and support checks happen at the call, before anything is queued.
    assert_eq!(
        queue_platform_action(app.world(), c.toggle.to_bits(), "tickle", None),
        Err(PlatformBridgeError::BadAction("tickle".to_owned()))
    );
    assert_eq!(
        queue_platform_action(app.world(), c.toggle.to_bits(), "increment", None),
        Err(PlatformBridgeError::Unsupported)
    );
    assert_eq!(
        queue_platform_action(app.world(), c.input.to_bits(), "set_value", None),
        Err(PlatformBridgeError::MissingValue)
    );
    assert_eq!(
        queue_platform_action(app.world(), u64::MAX - 7, "click", None),
        Err(PlatformBridgeError::UnknownNode(u64::MAX - 7))
    );
    assert!(!bridge.has_actions());

    let queued = queue_platform_action(app.world(), c.toggle.to_bits(), "click", None).unwrap();
    assert_eq!(queued["maps_to"].as_str(), Some("press"));
    let queued = queue_platform_action(
        app.world(),
        c.input.to_bits(),
        "set_value",
        Some(PlatformActionValue::Text("Ada".to_owned())),
    )
    .unwrap();
    assert_eq!(queued["maps_to"].as_str(), Some("set_text"));
    app.update();
    assert!(app.world().get::<Toggle>(c.toggle).unwrap().checked);
    assert_eq!(app.world().get::<TextInput>(c.input).unwrap().text, "Ada");
    app.update();
    let tree = platform_tree_json(app.world()).unwrap();
    let toggle = tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"].as_u64() == Some(c.toggle.to_bits()))
        .cloned()
        .unwrap();
    assert_eq!(toggle["toggled"].as_str(), Some("true"));

    let left = set_connected(app.world(), false).unwrap();
    assert_eq!(left["active"].as_bool(), Some(false));
}

#[test]
fn another_backend_means_the_bridge_is_off() {
    use regia_ui::a11y_platform::{PlatformBridgeError, platform_tree_json};
    let mut app = App::new();
    app.insert_resource(TextSystem::embedded_only());
    app.insert_resource(ScreenReader::with_backend(Box::new(
        regia_ui::a11y::NullScreenReader,
    )));
    app.add_plugins(UiPlugin::code_only());
    assert!(matches!(
        platform_tree_json(app.world()),
        Err(PlatformBridgeError::Off(_))
    ));
    assert!(app.world().get_resource::<AccessKitBridge>().is_none());
}

#[test]
fn the_default_install_puts_the_bridge_behind_the_screen_reader() {
    let mut app = App::new();
    app.insert_resource(TextSystem::embedded_only());
    app.add_plugins(UiPlugin::code_only());
    with_input_messages(&mut app);
    assert_eq!(
        app.world().resource::<ScreenReader>().backend().name(),
        "accesskit"
    );
    let bridge = app.world().resource::<AccessKitBridge>().clone();
    // No window, so no OS adapter: the platform tree still fills once connected.
    assert_eq!(bridge.adapter_name(), None);
    root_with_button(app.world_mut());
    bridge.activate();
    app.update();
    assert!(bridge.platform_tree().nodes.len() >= 4);
}

fn root_with_button(world: &mut World) {
    let root = world.spawn((UiRoot::default(), UiNode)).id();
    let button = world
        .spawn((UiNode, Button, Focusable::default(), ChildOf(root)))
        .id();
    world.spawn((UiNode, Text::new("Play"), ChildOf(button)));
}

// The controls screen as a screen reader meets it (lane v05-a11y-runtime-b, plan rows C10 and
// C12): the First Person template's frosted rebinding screen, its accessible names and actions
// as the platform adapter receives them, and axis tuning set from the platform that survives a
// restart. Still a recording adapter: this is not a run against a real reader.

mod controls_screen {
    use std::path::Path;
    use std::time::Duration;

    use accesskit::{Action, NodeId, Role, Toggled};
    use regia_app::prelude::*;
    use regia_input::prelude::*;
    use regia_input::{AxisTuning, RebindSlot, axis_tuning};
    use regia_platform::accessibility::{AccessKitBridge, PlatformTree};
    use regia_platform::{
        ButtonState, DirMount, GamepadButton, GamepadButtonChanged, GamepadConnected, MountPoint, TouchInput,
        TouchPhase, Vfs,
    };
    use regia_ui::a11y::ScreenReader;
    use regia_ui::accesskit_bridge::AccessKitBackend;
    use regia_ui::focus::FocusState;
    use regia_ui::prelude::*;
    use regia_ui::style::{Align, FlexDirection};
    use regia_ui::widgets::{Slider, Toggle};
    use regia_ui::{
        BackdropBlur, BackgroundKind, ComputedNode, LayoutDirty, NodeName, RebindPanel, RebindPanelOptions,
        UiScales,
    };

    use super::{RecordingAdapter, action, connect, node, with_input_messages};

    fn config() -> InputConfig {
        InputConfig::new()
            .action("Jump", ActionKind::Button)
            .action("Move", ActionKind::Axis2D)
            .action("Look", ActionKind::Axis2D)
            .binding(Binding::new("Jump", InputSource::key("Space")))
            .binding(Binding::new(
                "Jump",
                InputSource::gamepad_button(GamepadButton::South),
            ))
            .binding(Binding::new(
                "Move",
                InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
            ))
            .binding(
                Binding::new("Move", InputSource::gamepad_stick(Stick::Left))
                    .with_modifier(DeadZone::radial(0.15, 1.0)),
            )
            .binding(Binding::new("Look", InputSource::MouseDelta))
            .binding(Binding::new("Look", InputSource::gamepad_stick(Stick::Right)))
    }

    /// The template's controls screen: a `RebindPanel` with every option inside a frosted panel,
    /// player overrides on `user://` in `dir`, behind the AccessKit bridge.
    fn screen(dir: &Path, blur: f32) -> (App, AccessKitBridge) {
        screen_with(dir, blur, false)
    }

    /// [`screen`], or with `pause` the same frosted panel as an open modal dialog over a HUD that
    /// holds one `hud.resume` button (a pause menu's controls page).
    fn screen_with(dir: &Path, blur: f32, pause: bool) -> (App, AccessKitBridge) {
        let bridge = AccessKitBridge::new();
        bridge.attach_adapter("recording");
        let mut app = App::new();
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::User, DirMount::writable(dir.to_path_buf()));
        app.insert_resource(vfs);
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
            1.0 / 60.0,
        )));
        app.insert_resource(TextSystem::embedded_only());
        app.insert_resource(bridge.clone());
        app.insert_resource(ScreenReader::with_backend(Box::new(
            AccessKitBackend::with_budget(bridge.clone(), 4096),
        )));
        app.add_plugins(UiPlugin::code_only());
        with_input_messages(&mut app);
        app.add_plugins(InputPlugin::new(config()));
        let world = app.world_mut();
        world.insert_resource(UiScales {
            window_size: [1280.0, 800.0],
            scale_factor: 1.0,
        });
        if pause {
            let hud = world.spawn((UiRoot::default(), UiNode)).id();
            let resume = world
                .spawn((
                    UiNode,
                    Button,
                    Focusable::default(),
                    NodeName("hud.resume".to_owned()),
                    ChildOf(hud),
                ))
                .id();
            world.spawn((UiNode, Text::new("Resume"), ChildOf(resume)));
        }
        let root = world
            .spawn((
                UiRoot::default(),
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                    justify_content: JustifyContent::Center,
                    align_items: Align::Center,
                    ..Default::default()
                }),
            ))
            .id();
        let glass = world
            .spawn((
                UiNode,
                NodeName("controls.panel".to_owned()),
                Background {
                    kind: BackgroundKind::Color(UiColor([0.05, 0.07, 0.1, 0.55])),
                },
                NodeStyle::new(Style {
                    direction: FlexDirection::Column,
                    ..Default::default()
                }),
            ))
            .id();
        if blur > 0.0 {
            world.entity_mut(glass).insert(BackdropBlur { radius: blur });
        }
        if pause {
            world.entity_mut(glass).insert(Modal::default());
        }
        let panel = world
            .spawn((
                UiNode,
                RebindPanel {
                    context: "Gameplay".into(),
                    actions: Vec::new(),
                },
                RebindPanelOptions {
                    per_device: true,
                    axis_tuning: true,
                    profiles: vec!["player1".into()],
                },
            ))
            .id();
        world.entity_mut(glass).add_child(panel);
        world.entity_mut(root).add_child(glass);
        world.write_message(GamepadConnected {
            device: 1,
            name: "Pad".into(),
        });
        for _ in 0..3 {
            app.update();
            app.world_mut().resource_mut::<LayoutDirty>().mark();
        }
        app.update();
        (app, bridge)
    }

    fn named(app: &mut App, name: &str) -> Entity {
        app.world_mut()
            .query::<(Entity, &NodeName)>()
            .iter(app.world())
            .find(|(_, n)| n.0 == name)
            .map(|(e, _)| e)
            .unwrap_or_else(|| panic!("no node named {name}"))
    }

    fn tuning(app: &App, action: &str, slot: RebindSlot) -> AxisTuning {
        axis_tuning(app.world().resource::<InputConfig>(), "Gameplay", action, slot).expect("tuning")
    }

    /// What a reader speaks for each control it can focus: `name | description`, by node id.
    fn spoken(bridge: &AccessKitBridge) -> Vec<(NodeId, String)> {
        let tree = bridge.platform_tree();
        let mut out: Vec<(NodeId, String)> = tree
            .nodes
            .iter()
            .filter(|(_, n)| n.supports_action(Action::Focus))
            .map(|(id, n)| {
                (
                    *id,
                    format!(
                        "{} | {}",
                        n.label().unwrap_or_default(),
                        n.description().unwrap_or_default()
                    ),
                )
            })
            .collect();
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    fn settle(app: &mut App, bridge: &AccessKitBridge, adapter: &mut RecordingAdapter) {
        for _ in 0..3 {
            app.update();
        }
        adapter.drive(bridge);
    }

    /// ADR-4932, from the TEST Pixel 9 Pro (2026-11-09): TalkBack reads only the label through
    /// `accesskit_android`, which drops the description. The label alone is ambiguous on this
    /// screen; what the Android adapter is handed must not be.
    #[test]
    fn on_android_every_control_reads_uniquely_from_its_label_alone() {
        let dir = regia_test_utils::unique_temp_dir("a11y-controls-android-labels");
        let (mut app, bridge) = screen(&dir, 8.0);
        let mut adapter = RecordingAdapter::default();
        connect(&mut app, &bridge, &mut adapter);

        let labels = |update: &accesskit::TreeUpdate| -> Vec<String> {
            let mut out: Vec<String> = update
                .nodes
                .iter()
                .filter(|(_, n)| n.supports_action(Action::Focus))
                .map(|(_, n)| n.label().unwrap_or_default().to_owned())
                .collect();
            out.sort();
            out
        };
        let full = bridge.platform_tree().full_update().expect("a full tree");
        let unfolded = labels(&full);
        let mut distinct = unfolded.clone();
        distinct.dedup();
        assert!(
            distinct.len() < unfolded.len(),
            "the screen no longer needs the fold: {unfolded:#?}"
        );

        let folded = regia_platform::accessibility::fold_descriptions_into_labels(full);
        let read = labels(&folded);
        assert!(read.len() >= 20, "{read:#?}");
        let mut distinct = read.clone();
        distinct.dedup();
        assert_eq!(distinct.len(), read.len(), "labels that read the same: {read:#?}");
        assert!(read.iter().all(|l| !l.trim().is_empty()), "{read:#?}");
        for expected in [
            "Invert Y, Move, gamepad",
            "Invert Y, Look, gamepad",
            "Dead zone, Move, gamepad",
            "Left Stick, Move, gamepad",
            "W/S/A/D, Move, keyboard and mouse",
            "Reset, Move",
            "Save, Profile player1",
        ] {
            assert!(read.iter().any(|l| l == expected), "no {expected:?} in {read:#?}");
        }
        // The fold is the Android adapter's alone: the platform tree keeps both fields.
        let invert = node(
            &bridge,
            named(&mut app, "rebind.Gameplay.Move.tuning.gamepad.invert_y"),
        );
        assert_eq!(
            (invert.label(), invert.description()),
            (Some("Invert Y"), Some("Move, gamepad"))
        );
    }

    #[test]
    fn every_control_of_the_frosted_screen_reads_with_its_action_and_device() {
        let dir = regia_test_utils::unique_temp_dir("a11y-controls-names");
        let (mut app, bridge) = screen(&dir, 8.0);
        let mut adapter = RecordingAdapter::default();
        connect(&mut app, &bridge, &mut adapter);

        let spoken = spoken(&bridge);
        assert!(spoken.len() >= 20, "{spoken:#?}");
        for (id, words) in &spoken {
            assert!(!words.starts_with(" |"), "{id:?} has no name: {words}");
        }
        // Two controls that read the same leave a reader guessing which row and device they are.
        let mut words: Vec<&str> = spoken.iter().map(|(_, w)| w.as_str()).collect();
        words.dedup();
        assert_eq!(
            words.len(),
            spoken.len(),
            "controls that read the same: {spoken:#?}"
        );

        let invert = node(
            &bridge,
            named(&mut app, "rebind.Gameplay.Move.tuning.gamepad.invert_y"),
        );
        assert_eq!(
            (invert.role(), invert.label(), invert.description()),
            (Role::CheckBox, Some("Invert Y"), Some("Move, gamepad"))
        );
        assert_eq!(invert.toggled(), Some(Toggled::False));
        assert!(invert.supports_action(Action::Click));
        let zone = node(
            &bridge,
            named(&mut app, "rebind.Gameplay.Move.tuning.gamepad.dead_zone"),
        );
        assert_eq!(
            (zone.label(), zone.description()),
            (Some("Dead zone"), Some("Move, gamepad"))
        );
        assert!((zone.numeric_value().expect("value") - 0.15).abs() < 1e-6);
        assert!(zone.supports_action(Action::Increment) && zone.supports_action(Action::Decrement));
        let capture = node(&bridge, named(&mut app, "rebind.Gameplay.Move.gamepad"));
        assert_eq!(
            (capture.role(), capture.label(), capture.description()),
            (Role::Button, Some("Left Stick"), Some("Move, gamepad"))
        );
        let keys = node(&bridge, named(&mut app, "rebind.Gameplay.Move.keyboard_mouse"));
        assert_eq!(
            (keys.label(), keys.description()),
            (Some("W/S/A/D"), Some("Move, keyboard and mouse"))
        );
        let reset = node(&bridge, named(&mut app, "rebind.Gameplay.Move.reset"));
        assert_eq!(
            (reset.label(), reset.description()),
            (Some("Reset"), Some("Move"))
        );
        let save = node(&bridge, named(&mut app, "rebind.Gameplay.profile.player1.save"));
        assert_eq!(
            (save.label(), save.description()),
            (Some("Save"), Some("Profile player1"))
        );

        // The frosted panel is a backdrop, not a control: nothing to focus or press on it.
        let glass = named(&mut app, "controls.panel");
        assert!(
            bridge
                .platform_tree()
                .get(NodeId(glass.to_bits()))
                .is_none_or(|n| PlatformTree::actions(n).is_empty())
        );
        assert_eq!(bridge.stats().invalid_updates, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_blur_changes_nothing_a_reader_or_the_focus_order_sees() {
        let dirs = [
            regia_test_utils::unique_temp_dir("a11y-controls-clear"),
            regia_test_utils::unique_temp_dir("a11y-controls-frosted"),
        ];
        let mut runs = Vec::new();
        for (dir, blur) in dirs.iter().zip([0.0, 12.0]) {
            let (mut app, bridge) = screen(dir, blur);
            let mut adapter = RecordingAdapter::default();
            connect(&mut app, &bridge, &mut adapter);
            let words: Vec<String> = spoken(&bridge).into_iter().map(|(_, w)| w).collect();
            // Tab through the whole screen: the order of names the reader hears.
            let mut order = Vec::new();
            for _ in 0..words.len() {
                super::press_key(&mut app, "Tab");
                adapter.drive(&bridge);
                order.push(adapter.focus_label().unwrap_or_default());
            }
            runs.push((words, order));
        }
        assert_eq!(runs[0].0, runs[1].0, "the blur changed the accessible names");
        assert_eq!(runs[0].1, runs[1].1, "the blur changed the Tab order");
        assert!(runs[1].1.iter().all(|l| !l.is_empty()), "{:?}", runs[1].1);
        for dir in &dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn keyboard_pad_and_touch_reach_the_axis_controls_and_the_reader_hears_each_focus() {
        let dir = regia_test_utils::unique_temp_dir("a11y-controls-nav");
        let (mut app, bridge) = screen(&dir, 8.0);
        let mut adapter = RecordingAdapter::default();
        connect(&mut app, &bridge, &mut adapter);
        let invert = named(&mut app, "rebind.Gameplay.Look.tuning.gamepad.invert_y");
        let zone = named(&mut app, "rebind.Gameplay.Look.tuning.gamepad.dead_zone");
        let focusables = spoken(&bridge).len();

        // Tab reaches the toggle, and the platform's focus names it.
        let mut reached = false;
        for _ in 0..focusables {
            super::press_key(&mut app, "Tab");
            adapter.drive(&bridge);
            if app.world().resource::<FocusState>().focused == Some(invert) {
                reached = true;
                break;
            }
        }
        assert!(reached, "Tab never reached the Look gamepad invert toggle");
        assert_eq!(adapter.last().focus, NodeId(invert.to_bits()));
        assert_eq!(adapter.focus_label().as_deref(), Some("Invert Y"));
        // Enter flips it like a click; the reader hears the new state.
        super::press_key(&mut app, "Enter");
        settle(&mut app, &bridge, &mut adapter);
        assert!(app.world().get::<Toggle>(invert).unwrap().checked);
        assert!(tuning(&app, "Look", RebindSlot::Gamepad).invert_y);
        assert_eq!(node(&bridge, invert).toggled(), Some(Toggled::True));

        // The D-pad walks on; within a full lap it reaches the dead zone slider.
        let mut reached = false;
        for _ in 0..focusables {
            for state in [ButtonState::Pressed, ButtonState::Released] {
                app.world_mut().write_message(GamepadButtonChanged {
                    device: 1,
                    button: GamepadButton::DPadRight,
                    state,
                    value: 1.0,
                });
                app.update();
            }
            adapter.drive(&bridge);
            if app.world().resource::<FocusState>().focused == Some(zone) {
                reached = true;
                break;
            }
        }
        assert!(reached, "the D-pad never reached the Look gamepad dead zone");
        assert_eq!(adapter.focus_label().as_deref(), Some("Dead zone"));

        // A finger tap on the invert toggle turns it back off.
        let rect = app.world().get::<ComputedNode>(invert).expect("laid out").rect;
        let at = (
            f64::from(rect[0] + rect[2] * 0.5),
            f64::from(rect[1] + rect[3] * 0.5),
        );
        for phase in [TouchPhase::Started, TouchPhase::Ended] {
            app.world_mut().write_message(TouchInput {
                window: Entity::PLACEHOLDER,
                id: 7,
                phase,
                position: at,
                timestamp: Default::default(),
            });
            app.update();
        }
        settle(&mut app, &bridge, &mut adapter);
        assert!(!tuning(&app, "Look", RebindSlot::Gamepad).invert_y);
        assert_eq!(node(&bridge, invert).toggled(), Some(Toggled::False));
        assert_eq!(bridge.stats().invalid_updates, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn axis_tuning_set_from_the_platform_survives_a_restart_and_reads_back() {
        let dir = regia_test_utils::unique_temp_dir("a11y-controls-persist");
        {
            let (mut app, bridge) = screen(&dir, 8.0);
            let mut adapter = RecordingAdapter::default();
            connect(&mut app, &bridge, &mut adapter);
            let invert = named(&mut app, "rebind.Gameplay.Move.tuning.gamepad.invert_y");
            let zone = named(&mut app, "rebind.Gameplay.Move.tuning.gamepad.dead_zone");
            let look_zone = named(&mut app, "rebind.Gameplay.Look.tuning.gamepad.dead_zone");
            action(&bridge, invert, Action::Click, None);
            action(&bridge, zone, Action::Increment, None);
            // Look's stick starts with no dead zone: two steps of 0.05.
            action(&bridge, look_zone, Action::Increment, None);
            action(&bridge, look_zone, Action::Increment, None);
            settle(&mut app, &bridge, &mut adapter);
            let mv = tuning(&app, "Move", RebindSlot::Gamepad);
            assert!(mv.invert_y && !mv.invert_x);
            assert!((mv.dead_zone.expect("zone") - 0.2).abs() < 1e-6, "{mv:?}");
            let look = tuning(&app, "Look", RebindSlot::Gamepad);
            assert!((look.dead_zone.expect("zone") - 0.1).abs() < 1e-6, "{look:?}");
            assert!(
                !tuning(&app, "Move", RebindSlot::KeyboardMouse).invert_y,
                "the other device keeps its tuning"
            );
            assert_eq!(node(&bridge, invert).toggled(), Some(Toggled::True));
            assert!((node(&bridge, zone).numeric_value().unwrap() - 0.2).abs() < 1e-6);
            let requests: Vec<_> = app
                .world()
                .resource::<ScreenReader>()
                .recent_requests()
                .map(|(_, r)| *r)
                .collect();
            assert!(requests.iter().all(Result::is_ok), "{requests:?}");
            assert!(dir.join("input_overrides.ron").is_file(), "saved at once");
        }

        // A new session on the same user directory: the bindings and what the reader hears.
        let (mut app, bridge) = screen(&dir, 8.0);
        let mut adapter = RecordingAdapter::default();
        connect(&mut app, &bridge, &mut adapter);
        let mv = tuning(&app, "Move", RebindSlot::Gamepad);
        assert!(mv.invert_y);
        assert!((mv.dead_zone.expect("zone") - 0.2).abs() < 1e-6, "{mv:?}");
        let invert = named(&mut app, "rebind.Gameplay.Move.tuning.gamepad.invert_y");
        let zone = named(&mut app, "rebind.Gameplay.Move.tuning.gamepad.dead_zone");
        let look_zone = named(&mut app, "rebind.Gameplay.Look.tuning.gamepad.dead_zone");
        assert!(app.world().get::<Toggle>(invert).unwrap().checked);
        assert_eq!(node(&bridge, invert).toggled(), Some(Toggled::True));
        assert!((app.world().get::<Slider>(zone).unwrap().value - 0.2).abs() < 1e-6);
        assert!((node(&bridge, zone).numeric_value().unwrap() - 0.2).abs() < 1e-6);
        assert!((node(&bridge, look_zone).numeric_value().unwrap() - 0.1).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The focusable controls' rects, by name.
    fn control_rects(app: &mut App, bridge: &AccessKitBridge) -> Vec<(String, [f32; 4])> {
        let ids: Vec<NodeId> = spoken(bridge).into_iter().map(|(id, _)| id).collect();
        let mut out: Vec<(String, [f32; 4])> = app
            .world_mut()
            .query::<(Entity, &NodeName, &ComputedNode)>()
            .iter(app.world())
            .filter(|(e, ..)| ids.contains(&NodeId(e.to_bits())))
            .map(|(_, n, c)| (n.0.clone(), c.rect))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The phone flow: a notch and gesture-bar safe area, text at 200 %, reduced motion, then a
    /// right-to-left locale. Every control stays inside the safe area, keeps its name and stays
    /// reachable, and the audit finds no unnamed, unreachable, clipped or motion defect.
    #[test]
    fn under_a_safe_area_doubled_text_reduced_motion_and_rtl_every_control_stays_inside_and_reachable() {
        use regia_ui::a11y_audit::{AuditOptions, AuditProfile, AuditRule, AuditSeverity, audit_ui};
        use regia_ui::{Localization, SafeAreaPadding, UiAccessibility, UiRoot};
        const INSETS: [u32; 4] = [96, 0, 48, 40]; // left notch, right edge, gesture bar
        let dir = regia_test_utils::unique_temp_dir("a11y-controls-phone");
        let (mut app, bridge) = screen(&dir, 8.0);
        let mut adapter = RecordingAdapter::default();
        connect(&mut app, &bridge, &mut adapter);
        let before: Vec<String> = spoken(&bridge).into_iter().map(|(_, w)| w).collect();

        app.insert_resource(regia_platform::SafeArea {
            left: INSETS[0],
            top: INSETS[1],
            right: INSETS[2],
            bottom: INSETS[3],
        });
        let roots: Vec<Entity> = app
            .world_mut()
            .query_filtered::<Entity, With<UiRoot>>()
            .iter(app.world())
            .collect();
        // A touch screen lays out in device-independent px, as the template's touch layout does. Under
        // the desktop 1920x1080 reference this 1280x800 screen ran at about 0.67, so its 44 UI px
        // controls were 29 px; the touch check below passed then only because the audit measured UI px
        // (ADR-4932).
        for root in roots {
            let mut root = app.world_mut().entity_mut(root);
            root.insert(SafeAreaPadding);
            if let Some(mut ui) = root.get_mut::<UiRoot>() {
                ui.scale_mode = regia_ui::ScaleMode::ConstantPhysicalSize;
            }
        }
        {
            let mut a11y = app.world_mut().resource_mut::<UiAccessibility>();
            a11y.text_scale = 2.0;
            a11y.reduced_motion = true;
        }
        let inside = |rects: &[(String, [f32; 4])], what: &str| {
            let [w, h] = [1280.0_f32, 800.0];
            let safe = [
                INSETS[0] as f32,
                INSETS[1] as f32,
                w - INSETS[2] as f32,
                h - INSETS[3] as f32,
            ];
            for (name, r) in rects {
                assert!(
                    r[0] >= safe[0] - 0.5
                        && r[1] >= safe[1] - 0.5
                        && r[0] + r[2] <= safe[2] + 0.5
                        && r[1] + r[3] <= safe[3] + 0.5,
                    "{what}: {name} at {r:?} leaves the safe area {safe:?}"
                );
            }
        };
        for locale in ["en-US", "he-IL"] {
            {
                let mut loc = app.world_mut().resource_mut::<Localization>();
                loc.selected = locale.to_owned();
                loc.generation.0 += 1;
            }
            for _ in 0..3 {
                app.world_mut().resource_mut::<LayoutDirty>().mark();
                app.update();
            }
            adapter.drive(&bridge);
            let rects = control_rects(&mut app, &bridge);
            assert_eq!(rects.len(), before.len(), "{locale}: {rects:#?}");
            inside(&rects, locale);
            let now: Vec<String> = spoken(&bridge).into_iter().map(|(_, w)| w).collect();
            assert_eq!(now, before, "{locale}: the names changed with the layout");
            // Tab still walks every control once per lap.
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..before.len() {
                super::press_key(&mut app, "Tab");
                if let Some(f) = app.world().resource::<FocusState>().focused {
                    seen.insert(f);
                }
            }
            assert_eq!(seen.len(), before.len(), "{locale}: Tab missed controls");

            let options = AuditOptions {
                rules: vec![
                    AuditRule::UnnamedInteractive,
                    AuditRule::FocusUnreachable,
                    AuditRule::TextClippedAtScale,
                    AuditRule::MotionIgnoresSetting,
                ],
                text_scale: 2.0,
                ..AuditOptions::default()
            };
            let report = audit_ui(app.world_mut(), &options);
            assert!(report.reduced_motion);
            let defects: Vec<_> = report
                .findings
                .iter()
                .filter(|f| f.severity != AuditSeverity::Info)
                .collect();
            assert!(defects.is_empty(), "{locale}:\n{}", report.to_lines());
            // A fingertip on the phone: every control meets the touch target.
            let touch = audit_ui(
                app.world_mut(),
                &AuditOptions {
                    profile: AuditProfile::Touch,
                    rules: vec![AuditRule::TargetSize],
                    text_scale: 2.0,
                    ..AuditOptions::default()
                },
            );
            assert!(
                touch.findings.iter().all(|f| f.severity == AuditSeverity::Info),
                "{locale} touch:\n{}",
                touch.to_lines()
            );
        }
        assert_eq!(bridge.stats().invalid_updates, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn pad_press(app: &mut App, button: GamepadButton) {
        for state in [ButtonState::Pressed, ButtonState::Released] {
            app.world_mut().write_message(GamepadButtonChanged {
                device: 1,
                button,
                state,
                value: if state == ButtonState::Pressed { 1.0 } else { 0.0 },
            });
            app.update();
        }
    }

    /// The press a capture takes belongs to the capture: it neither closes the pause dialog nor
    /// moves focus, so a player (and their reader) stays on the row they are rebinding, and the
    /// dialog still closes from the keyboard once no capture runs.
    #[test]
    fn a_capture_on_the_pause_dialog_keeps_the_dialog_open_and_the_focus_on_its_row() {
        let dir = regia_test_utils::unique_temp_dir("a11y-controls-pause");
        let (mut app, bridge) = screen_with(&dir, 8.0, true);
        let mut adapter = RecordingAdapter::default();
        connect(&mut app, &bridge, &mut adapter);
        let dialog = named(&mut app, "controls.panel");
        let resume = named(&mut app, "hud.resume");
        assert!(node(&bridge, dialog).is_modal());
        assert!(
            PlatformTree::actions(&node(&bridge, resume)).is_empty(),
            "the HUD under the dialog is inert"
        );
        let open = |app: &App| app.world().get::<Modal>(dialog).is_some_and(|m| m.open);
        let focused = |app: &App| app.world().resource::<FocusState>().focused;
        let label = |bridge: &AccessKitBridge, e: Entity| node(bridge, e).label().map(str::to_owned);

        // The reader starts a gamepad capture; the button reads the prompt.
        let pad = named(&mut app, "rebind.Gameplay.Jump.gamepad");
        action(&bridge, pad, Action::Focus, None);
        action(&bridge, pad, Action::Click, None);
        settle(&mut app, &bridge, &mut adapter);
        assert_eq!(focused(&app), Some(pad));
        assert_eq!(
            label(&bridge, pad).as_deref(),
            Some("Press a button or move a stick...")
        );
        // B is the pad's back button, and the capture takes it as Jump's new button.
        pad_press(&mut app, GamepadButton::East);
        settle(&mut app, &bridge, &mut adapter);
        assert!(open(&app), "binding B closed the pause dialog");
        assert_eq!(focused(&app), Some(pad));
        let bound = label(&bridge, pad).unwrap_or_default();
        assert!(!bound.starts_with("Press"), "{bound}");
        assert_eq!(
            regia_input::binding_label_in(
                app.world().resource::<InputConfig>(),
                "Gameplay",
                "Jump",
                RebindSlot::Gamepad
            )
            .as_deref(),
            Some(bound.as_str())
        );

        // A keyboard capture takes Tab without moving focus.
        let keys = named(&mut app, "rebind.Gameplay.Jump.keyboard_mouse");
        action(&bridge, keys, Action::Focus, None);
        action(&bridge, keys, Action::Click, None);
        settle(&mut app, &bridge, &mut adapter);
        assert!(label(&bridge, keys).is_some_and(|l| l.starts_with("Press")));
        super::press_key(&mut app, "Tab");
        settle(&mut app, &bridge, &mut adapter);
        assert_eq!(focused(&app), Some(keys), "the captured Tab moved focus");
        assert_eq!(adapter.last().focus, NodeId(keys.to_bits()));
        assert_eq!(label(&bridge, keys).as_deref(), Some("Tab"));

        // Escape leaves a capture and nothing else.
        action(&bridge, keys, Action::Click, None);
        settle(&mut app, &bridge, &mut adapter);
        assert!(label(&bridge, keys).is_some_and(|l| l.starts_with("Press")));
        super::press_key(&mut app, "Escape");
        settle(&mut app, &bridge, &mut adapter);
        assert!(open(&app), "the Escape that left the capture closed the dialog");
        assert_eq!(focused(&app), Some(keys));
        assert_eq!(label(&bridge, keys).as_deref(), Some("Tab"));

        // With no capture running, Escape closes the dialog and the HUD can be reached again.
        super::press_key(&mut app, "Escape");
        settle(&mut app, &bridge, &mut adapter);
        assert!(!open(&app), "Escape no longer closes the dialog");
        assert!(node(&bridge, resume).supports_action(Action::Click));
        assert_eq!(bridge.stats().invalid_updates, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// One key press and release, a frame each, as winit sends them.
fn press_key(app: &mut App, name: &str) {
    for state in [
        regia_platform::ButtonState::Pressed,
        regia_platform::ButtonState::Released,
    ] {
        app.world_mut().write_message(regia_platform::KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: name.into(),
            logical_key: name.into(),
            state,
            repeat: false,
            timestamp: Default::default(),
        });
        app.update();
    }
}
