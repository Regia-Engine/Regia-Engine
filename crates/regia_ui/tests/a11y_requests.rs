// SPDX-License-Identifier: MPL-2.0
//! Screen-reader requests, localizable labels and hints, and the screen-changed signal (ADR-1621).
//! Headless, no GPU, no clock: requests come through a `RecordingReader` and the sync runs on plain
//! `app.update()` calls; scroll cases drive `apply_request` on a bare world with hand-set layout.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_ui::a11y::atspi::{self, AtSpiStub};
use regia_ui::a11y::{
    A11yAction, A11yEvent, A11yReject, A11yRequest, A11yRole, Accessible, PageDirection, RecordingHandle,
    RecordingReader, ScreenReader, apply_request, build_a11y_tree,
};
use regia_ui::prelude::*;
use regia_ui::{FocusState, Focused, InputFilter, LayerInputMode};

fn headless_with(backend: Box<dyn regia_ui::a11y::ScreenReaderBackend>) -> App {
    let mut app = App::new();
    app.insert_resource(TextSystem::embedded_only());
    app.insert_resource(ScreenReader::with_backend(backend));
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app
}

fn headless() -> (App, RecordingHandle) {
    let (reader, handle) = RecordingReader::new();
    (headless_with(Box::new(reader)), handle)
}

struct Controls {
    start: Entity,
    toggle: Entity,
    slider: Entity,
    input: Entity,
}

fn controls(world: &mut World) -> Controls {
    let root = world.spawn((UiRoot::default(), UiNode)).id();
    let start = world
        .spawn((UiNode, Button, Focusable::default(), ChildOf(root)))
        .id();
    world.spawn((UiNode, Text::new("Start"), ChildOf(start)));
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
    // `TextInput` has private fields, so it is built by mutation, not struct-update syntax.
    let mut field = TextInput::default();
    field.placeholder = "Player name".to_owned();
    let input = world.spawn((UiNode, field, ChildOf(root))).id();
    Controls {
        start,
        toggle,
        slider,
        input,
    }
}

fn outcome(handle: &RecordingHandle) -> Vec<(A11yAction, Result<(), A11yReject>)> {
    handle
        .results()
        .into_iter()
        .map(|(r, res)| (r.action, res))
        .collect()
}

#[test]
fn press_flips_a_check_box_through_the_click_path() {
    let (mut app, handle) = headless();
    let c = controls(app.world_mut());
    app.update();
    handle.request(c.toggle.to_bits(), A11yAction::Press);
    app.update();
    assert!(app.world().get::<Toggle>(c.toggle).unwrap().checked);
    assert_eq!(outcome(&handle), vec![(A11yAction::Press, Ok(()))]);
    // The reader sees the new state on the following sync.
    app.update();
    let node = app
        .world()
        .resource::<ScreenReader>()
        .tree()
        .get(c.toggle.to_bits())
        .cloned()
        .unwrap();
    assert_eq!(node.states.checked, Some(true));
    assert_eq!(node.spoken_summary(), "Subtitles, check box, checked");
}

#[test]
fn a_request_leaves_nothing_behind_when_no_reader_is_live() {
    // A hook with the null backend and exporting off: requests are never drained.
    let mut app = headless_with(Box::new(regia_ui::a11y::NullScreenReader));
    let c = controls(app.world_mut());
    app.update();
    assert!(!app.world().resource::<ScreenReader>().is_live());
    assert!(!app.world().get::<Toggle>(c.toggle).unwrap().checked);
    assert_eq!(
        app.world().resource::<ScreenReader>().recent_requests().count(),
        0
    );
}

#[test]
fn raise_and_lower_step_a_slider_by_its_own_step_and_stop_at_the_ends() {
    let (mut app, handle) = headless();
    let c = controls(app.world_mut());
    app.update();
    let id = c.slider.to_bits();
    handle.request(id, A11yAction::Raise);
    app.update();
    assert_eq!(app.world().get::<Slider>(c.slider).unwrap().value, 55.0);
    handle.request(id, A11yAction::Lower);
    handle.request(id, A11yAction::Lower);
    app.update();
    assert_eq!(app.world().get::<Slider>(c.slider).unwrap().value, 45.0);
    app.world_mut().get_mut::<Slider>(c.slider).unwrap().value = 99.0;
    handle.request(id, A11yAction::Raise);
    app.update();
    assert_eq!(app.world().get::<Slider>(c.slider).unwrap().value, 100.0);
    // A slider without a step moves a tenth of its range.
    app.world_mut().get_mut::<Slider>(c.slider).unwrap().step = 0.0;
    app.world_mut().get_mut::<Slider>(c.slider).unwrap().value = 20.0;
    handle.request(id, A11yAction::Raise);
    app.update();
    assert!((app.world().get::<Slider>(c.slider).unwrap().value - 30.0).abs() < 1e-3);
    // Raise on a button is refused.
    handle.request(c.start.to_bits(), A11yAction::Raise);
    app.update();
    assert_eq!(
        outcome(&handle).last(),
        Some(&(A11yAction::Raise, Err(A11yReject::Unsupported)))
    );
}

/// audit-seq (Medium): the step fraction was measured from `min`, so on a reversed slider
/// (`min` 10, `max` 0) every raise or lower landed on the low end.
#[test]
fn raise_and_lower_step_a_reversed_slider_in_value_space() {
    let (mut app, handle) = headless();
    let c = controls(app.world_mut());
    {
        let mut slider = app.world_mut().get_mut::<Slider>(c.slider).unwrap();
        slider.min = 10.0;
        slider.max = 0.0;
        slider.step = 1.0;
        slider.value = 5.0;
    }
    app.update();
    let id = c.slider.to_bits();
    handle.request(id, A11yAction::Raise);
    app.update();
    assert_eq!(app.world().get::<Slider>(c.slider).unwrap().value, 6.0);
    handle.request(id, A11yAction::Lower);
    handle.request(id, A11yAction::Lower);
    app.update();
    assert_eq!(app.world().get::<Slider>(c.slider).unwrap().value, 4.0);
    // The ends still stop it.
    app.world_mut().get_mut::<Slider>(c.slider).unwrap().value = 9.5;
    handle.request(id, A11yAction::Raise);
    app.update();
    assert_eq!(app.world().get::<Slider>(c.slider).unwrap().value, 10.0);
}

#[test]
fn set_text_replaces_the_text_and_respects_read_only_and_max_chars() {
    let (mut app, handle) = headless();
    let c = controls(app.world_mut());
    app.update();
    handle.request(c.input.to_bits(), A11yAction::SetText("Ada".to_owned()));
    app.update();
    assert_eq!(app.world().get::<TextInput>(c.input).unwrap().text, "Ada");
    app.world_mut().get_mut::<TextInput>(c.input).unwrap().max_chars = 2;
    handle.request(c.input.to_bits(), A11yAction::SetText("Grace".to_owned()));
    app.update();
    assert_eq!(app.world().get::<TextInput>(c.input).unwrap().text, "Gr");
    app.world_mut().get_mut::<TextInput>(c.input).unwrap().read_only = true;
    handle.request(c.input.to_bits(), A11yAction::SetText("Zed".to_owned()));
    app.update();
    assert_eq!(app.world().get::<TextInput>(c.input).unwrap().text, "Gr");
    assert_eq!(
        outcome(&handle).last().map(|(_, r)| *r),
        Some(Err(A11yReject::Unsupported))
    );
    // Set text on a button is refused too.
    handle.request(c.start.to_bits(), A11yAction::SetText("x".to_owned()));
    app.update();
    assert_eq!(
        outcome(&handle).last().map(|(_, r)| *r),
        Some(Err(A11yReject::Unsupported))
    );
}

/// Audit wave 5 (accesskit H3, M2): a screen reader's set-value request is held to what typing
/// would allow. Control characters go, the field's filter refuses text it would refuse when typed,
/// `max_chars` cuts whole grapheme clusters, and an oversized text is refused untouched.
#[test]
fn set_text_from_a_reader_is_sanitized_filtered_and_capped_like_typing() {
    let (mut app, handle) = headless();
    let c = controls(app.world_mut());
    app.update();
    handle.request(
        c.input.to_bits(),
        A11yAction::SetText("A\u{0}d\u{7}a\nB".to_owned()),
    );
    app.update();
    assert_eq!(app.world().get::<TextInput>(c.input).unwrap().text, "Ada B");
    // A base letter keeps its combining mark: "e\u{301}" is one cluster of two scalars.
    app.world_mut().get_mut::<TextInput>(c.input).unwrap().max_chars = 2;
    handle.request(c.input.to_bits(), A11yAction::SetText("xe\u{301}".to_owned()));
    app.update();
    assert_eq!(app.world().get::<TextInput>(c.input).unwrap().text, "x");
    app.world_mut().get_mut::<TextInput>(c.input).unwrap().max_chars = 0;
    app.world_mut().get_mut::<TextInput>(c.input).unwrap().filter = InputFilter::Integer;
    handle.request(c.input.to_bits(), A11yAction::SetText("4a".to_owned()));
    app.update();
    assert_eq!(app.world().get::<TextInput>(c.input).unwrap().text, "x");
    assert_eq!(
        outcome(&handle).last().map(|(_, r)| *r),
        Some(Err(A11yReject::Unsupported))
    );
    handle.request(c.input.to_bits(), A11yAction::SetText("-42".to_owned()));
    app.update();
    assert_eq!(app.world().get::<TextInput>(c.input).unwrap().text, "-42");
    let long = "7".repeat(regia_ui::a11y::MAX_SET_TEXT_CHARS + 1);
    handle.request(c.input.to_bits(), A11yAction::SetText(long));
    app.update();
    assert_eq!(app.world().get::<TextInput>(c.input).unwrap().text, "-42");
    assert_eq!(
        outcome(&handle).last().map(|(_, r)| *r),
        Some(Err(A11yReject::Unsupported))
    );
    let most = "7".repeat(regia_ui::a11y::MAX_SET_TEXT_CHARS);
    handle.request(c.input.to_bits(), A11yAction::SetText(most.clone()));
    app.update();
    assert_eq!(app.world().get::<TextInput>(c.input).unwrap().text, most);
}

#[test]
fn focus_moves_the_keyboard_focus_and_press_on_a_text_field_focuses_it() {
    let (mut app, handle) = headless();
    let c = controls(app.world_mut());
    app.world_mut().entity_mut(c.input).insert(Focusable::default());
    app.update();
    handle.request(c.start.to_bits(), A11yAction::Focus);
    app.update();
    assert_eq!(app.world().resource::<FocusState>().focused, Some(c.start));
    assert!(app.world().get::<Focused>(c.start).is_some());
    handle.request(c.input.to_bits(), A11yAction::Press);
    app.update();
    assert_eq!(app.world().resource::<FocusState>().focused, Some(c.input));
    assert!(app.world().get::<Focused>(c.start).is_none());
    // The sync reports where focus went.
    app.update();
    assert!(handle.events().iter().any(|e| matches!(
        e,
        A11yEvent::FocusChanged { to: Some(to), .. } if *to == c.input.to_bits()
    )));
    // A node that cannot take focus refuses.
    handle.request(c.slider.to_bits(), A11yAction::Focus);
    app.update();
    assert_eq!(
        outcome(&handle).last().map(|(_, r)| *r),
        Some(Err(A11yReject::Unsupported))
    );
}

#[test]
fn disabled_inert_and_unknown_nodes_are_refused_without_effect() {
    let (mut app, handle) = headless();
    let c = controls(app.world_mut());
    app.world_mut().entity_mut(c.toggle).insert(Disabled);
    app.update();
    handle.request(c.toggle.to_bits(), A11yAction::Press);
    // A despawned node and an id that was never an entity.
    let gone = app.world_mut().spawn(UiNode).id();
    app.world_mut().despawn(gone);
    handle.request(gone.to_bits(), A11yAction::Press);
    handle.request(u64::MAX, A11yAction::Press);
    // The button sits under an open dialog from here on.
    let dialog_root = app.world_mut().spawn((UiRoot::default(), UiNode)).id();
    let dialog = app
        .world_mut()
        .spawn((UiNode, Modal::default(), ChildOf(dialog_root)))
        .id();
    app.world_mut()
        .spawn((UiNode, Text::new("Quit?"), ChildOf(dialog)));
    regia_ui::layers::push_layer(app.world_mut(), dialog, LayerInputMode::UiOnly).unwrap();
    handle.request(c.start.to_bits(), A11yAction::Press);
    app.update();
    let results: Vec<_> = outcome(&handle).into_iter().map(|(_, r)| r).collect();
    assert_eq!(
        results,
        vec![
            Err(A11yReject::Disabled),
            Err(A11yReject::UnknownNode),
            Err(A11yReject::UnknownNode),
            Err(A11yReject::Inert),
        ]
    );
    assert!(!app.world().get::<Toggle>(c.toggle).unwrap().checked);
    assert_eq!(
        A11yReject::Inert.reason(),
        "node_inert",
        "reasons are protocol: lowercase snake_case"
    );
    for reject in [
        A11yReject::UnknownNode,
        A11yReject::Disabled,
        A11yReject::Inert,
        A11yReject::Unsupported,
        A11yReject::NotDismissable,
        A11yReject::NoScrollView,
    ] {
        assert!(
            reject
                .reason()
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_')
        );
    }
}

#[test]
fn close_dismisses_the_dialog_the_node_is_in_and_the_screen_change_is_reported() {
    let (mut app, handle) = headless();
    let c = controls(app.world_mut());
    app.update();
    handle.clear_events();
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
    // The open modal gets its layer automatically, and loses it when it closes (ADR-293). A layer
    // pushed by hand stays until it is popped, so closing the modal alone would not change the
    // screen.
    app.update();
    // Opening the dialog moved the screen: a bridge refocuses.
    assert!(handle.events().contains(&A11yEvent::ScreenChanged {
        from: None,
        to: Some(dialog.to_bits()),
    }));
    handle.clear_events();
    handle.request(ok.to_bits(), A11yAction::Close);
    app.update();
    assert!(!app.world().get::<Modal>(dialog).unwrap().open);
    app.update();
    assert!(handle.events().contains(&A11yEvent::ScreenChanged {
        from: Some(dialog.to_bits()),
        to: None,
    }));
    // The menu is reachable again.
    handle.request(c.toggle.to_bits(), A11yAction::Press);
    app.update();
    assert!(app.world().get::<Toggle>(c.toggle).unwrap().checked);
}

#[test]
fn a_dialog_that_ignores_cancel_cannot_be_dismissed_by_a_reader() {
    let (mut app, handle) = headless();
    controls(app.world_mut());
    let dialog_root = app.world_mut().spawn((UiRoot::default(), UiNode)).id();
    let dialog = app
        .world_mut()
        .spawn((
            UiNode,
            Modal {
                open: true,
                close_on_cancel: false,
            },
            ChildOf(dialog_root),
        ))
        .id();
    app.world_mut()
        .spawn((UiNode, Text::new("Saving"), ChildOf(dialog)));
    regia_ui::layers::push_layer(app.world_mut(), dialog, LayerInputMode::UiOnly).unwrap();
    app.update();
    handle.request(dialog.to_bits(), A11yAction::Close);
    app.update();
    assert!(app.world().get::<Modal>(dialog).unwrap().open);
    assert_eq!(
        outcome(&handle).last().map(|(_, r)| *r),
        Some(Err(A11yReject::NotDismissable))
    );
    // Close on a node that is in no dialog is unsupported.
    let (mut app, handle) = headless();
    let c = controls(app.world_mut());
    app.update();
    handle.request(c.start.to_bits(), A11yAction::Close);
    app.update();
    assert_eq!(
        outcome(&handle).last().map(|(_, r)| *r),
        Some(Err(A11yReject::Unsupported))
    );
}

#[test]
fn scrolling_requests_page_and_reveal_nodes() {
    let mut world = World::new();
    let root = world.spawn((UiRoot::default(), UiNode)).id();
    let view = world
        .spawn((
            UiNode,
            ScrollView::vertical(),
            ComputedNode {
                rect: [0.0, 0.0, 100.0, 100.0],
                content_size: [100.0, 1000.0],
                ..ComputedNode::default()
            },
            ChildOf(root),
        ))
        .id();
    let item = world
        .spawn((
            UiNode,
            Button,
            Focusable::default(),
            ComputedNode {
                rect: [0.0, 500.0, 100.0, 50.0],
                ..ComputedNode::default()
            },
            ChildOf(view),
        ))
        .id();
    world.spawn((UiNode, Text::new("Item 10"), ChildOf(item)));
    let offset = |world: &World| world.get::<ScrollOffset>(view).unwrap().0;
    let ask = |world: &mut World, node: Entity, action: A11yAction| {
        apply_request(
            world,
            &A11yRequest {
                node: node.to_bits(),
                action,
            },
        )
    };
    assert_eq!(
        ask(&mut world, view, A11yAction::Page(PageDirection::Down)),
        Ok(())
    );
    assert_eq!(offset(&world), [0.0, 90.0]);
    // Paging from a node inside the view pages the view.
    assert_eq!(
        ask(&mut world, item, A11yAction::Page(PageDirection::Down)),
        Ok(())
    );
    assert_eq!(offset(&world), [0.0, 180.0]);
    assert_eq!(ask(&mut world, view, A11yAction::Page(PageDirection::Up)), Ok(()));
    assert_eq!(offset(&world), [0.0, 90.0]);
    // Up past the start stops at the start; sideways does nothing on a vertical view.
    assert_eq!(ask(&mut world, view, A11yAction::Page(PageDirection::Up)), Ok(()));
    assert_eq!(ask(&mut world, view, A11yAction::Page(PageDirection::Up)), Ok(()));
    assert_eq!(offset(&world), [0.0, 0.0]);
    assert_eq!(
        ask(&mut world, view, A11yAction::Page(PageDirection::Right)),
        Ok(())
    );
    assert_eq!(offset(&world), [0.0, 0.0]);
    // Reveal the item: its end (550) must reach the view's end (100).
    assert_eq!(ask(&mut world, item, A11yAction::ScrollIntoView), Ok(()));
    assert_eq!(offset(&world), [0.0, 450.0]);
    // A node outside any scroll view has none to scroll; the view itself has no enclosing one.
    assert_eq!(
        ask(&mut world, view, A11yAction::ScrollIntoView),
        Err(A11yReject::NoScrollView)
    );
    let loose = world
        .spawn((UiNode, Button, Focusable::default(), ChildOf(root)))
        .id();
    assert_eq!(
        ask(&mut world, loose, A11yAction::Page(PageDirection::Down)),
        Err(A11yReject::NoScrollView)
    );
}

/// Two locales and a missing-in-French key.
fn localization() -> Localization {
    Localization::new(
        vec![
            (
                "en".to_owned(),
                "back-label = Back\nback-hint = Returns to the previous screen\n".to_owned(),
            ),
            ("fr".to_owned(), "back-label = Retour\n".to_owned()),
        ],
        Some("en".to_owned()),
    )
    .unwrap()
}

#[test]
fn override_labels_and_hints_follow_the_locale_and_the_diff_reports_it() {
    let mut world = World::new();
    world.insert_resource(localization());
    let root = world.spawn((UiRoot::default(), UiNode)).id();
    let back = world
        .spawn((
            UiNode,
            Button,
            Accessible::default()
                .with_label("Back (literal)")
                .with_label_key("back-label")
                .with_hint_key("back-hint"),
            ChildOf(root),
        ))
        .id();
    // A key nobody defined falls back to the literal.
    let help = world
        .spawn((
            UiNode,
            Button,
            Accessible::default()
                .with_label("Help")
                .with_label_key("no-such-key")
                .with_hint("Opens the manual"),
            ChildOf(root),
        ))
        .id();
    let english = build_a11y_tree(&mut world);
    let node = english.get(back.to_bits()).unwrap();
    assert_eq!(node.name, "Back");
    assert_eq!(node.hint, "Returns to the previous screen");
    assert_eq!(
        node.spoken_summary(),
        "Back, button, Returns to the previous screen"
    );
    let node = english.get(help.to_bits()).unwrap();
    assert_eq!(node.name, "Help");
    assert_eq!(node.spoken_summary(), "Help, button, Opens the manual");
    assert_eq!(node.role, Some(A11yRole::Button));

    assert!(world.resource_mut::<Localization>().set_locale("fr"));
    let french = build_a11y_tree(&mut world);
    let node = french.get(back.to_bits()).unwrap();
    assert_eq!(node.name, "Retour");
    // French lacks the hint key; the chain falls back to English.
    assert_eq!(node.hint, "Returns to the previous screen");
    let events = french.diff(&english);
    assert_eq!(events, vec![A11yEvent::NameChanged { id: back.to_bits() }]);

    // A key with no literal and no definition shows the table's missing-key text.
    let missing = world
        .spawn((
            UiNode,
            Button,
            Accessible::default().with_label_key("never-defined"),
            ChildOf(root),
        ))
        .id();
    let tree = build_a11y_tree(&mut world);
    assert!(
        tree.get(missing.to_bits())
            .unwrap()
            .name
            .contains("never-defined")
    );
}

#[test]
fn the_at_spi_seam_maps_calls_to_requests_and_answers_with_a_signal() {
    let path = atspi::object_path(42);
    assert_eq!(atspi::parse_object_path(&path), Some(42));
    assert_eq!(atspi::parse_object_path("/other/42"), None);
    let request = |action: &str, text: Option<&str>| atspi::request_from_call(&path, action, text);
    assert_eq!(
        request("click", None),
        Some(A11yRequest {
            node: 42,
            action: A11yAction::Press
        })
    );
    assert_eq!(
        request("increment", None).map(|r| r.action),
        Some(A11yAction::Raise)
    );
    assert_eq!(
        request("decrement", None).map(|r| r.action),
        Some(A11yAction::Lower)
    );
    assert_eq!(
        request("set-text", Some("Ada")).map(|r| r.action),
        Some(A11yAction::SetText("Ada".to_owned()))
    );
    assert_eq!(request("set-text", None), None);
    assert_eq!(
        request("dismiss", None).map(|r| r.action),
        Some(A11yAction::Close)
    );
    assert_eq!(
        request("page-down", None).map(|r| r.action),
        Some(A11yAction::Page(PageDirection::Down))
    );
    assert_eq!(request("levitate", None), None);

    let stub = AtSpiStub::new(true);
    let queue = stub.request_queue();
    let mut app = headless_with(Box::new(stub));
    let c = controls(app.world_mut());
    app.update();
    queue.push(atspi::request_from_call(&atspi::object_path(c.toggle.to_bits()), "click", None).unwrap());
    app.update();
    assert!(app.world().get::<Toggle>(c.toggle).unwrap().checked);
    let signals = app.world().resource::<ScreenReader>().backend().recent_events();
    assert!(
        signals
            .iter()
            .any(|s| s.starts_with("request:result") && s.ends_with("press ok")),
        "{signals:?}"
    );
}

#[test]
fn action_names_round_trip() {
    for name in A11yAction::NAMES {
        let action = A11yAction::parse(name, Some("x")).unwrap();
        assert_eq!(action.name(), name);
    }
    assert_eq!(A11yAction::parse("set_text", None), None);
    assert_eq!(A11yAction::parse("tickle", Some("x")), None);
}

/// audit-seq (Low): the tree builder recursed through `Children` without a bound, so a corrupt
/// hierarchy (a root and its child parenting each other) overflowed the stack on every live frame.
#[test]
fn a_children_cycle_is_walked_once() {
    let mut world = World::new();
    let root = world.spawn((UiRoot::default(), UiNode)).id();
    let a = world
        .spawn((
            UiNode,
            Button,
            Accessible::default().with_label("A"),
            ChildOf(root),
        ))
        .id();
    world.spawn((UiNode, Button, Accessible::default().with_label("B"), ChildOf(a)));
    // Corrupt: the root becomes a child of its own child.
    world.entity_mut(root).insert(ChildOf(a));
    let tree = build_a11y_tree(&mut world);
    let mut ids: Vec<u64> = tree.nodes.iter().map(|n| n.id).collect();
    let before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), before, "every node once");
    assert!(
        tree.nodes.iter().any(|n| n.name == "B"),
        "the rest of the tree is still built"
    );
}

/// Audit wave 5 (accesskit H1): the tree walk is depth-bounded, so a hierarchy 50,000 levels deep
/// (well past what an 8 MiB stack holds at one walk frame per level) builds a tree cut at
/// `MAX_A11Y_DEPTH` instead of overflowing the stack. Every level is focusable, so none is
/// flattened and the node count is exact.
#[test]
fn a_hierarchy_deeper_than_the_walk_limit_is_cut_and_does_not_overflow_the_stack() {
    let walk = std::thread::Builder::new()
        .stack_size(8 << 20)
        .spawn(|| {
            let mut world = World::new();
            let mut parent = world.spawn((UiRoot::default(), UiNode)).id();
            for _ in 0..50_000 {
                parent = world.spawn((UiNode, Focusable::default(), ChildOf(parent))).id();
            }
            build_a11y_tree(&mut world).nodes.len()
        })
        .unwrap();
    assert_eq!(walk.join().unwrap(), regia_ui::a11y::MAX_A11Y_DEPTH);
}

/// Audit accesskit M1 (bounded form): the exported tree has a node cap, so a UI of hundreds of
/// thousands of nodes cannot make every live frame rebuild, diff and clone all of them. A flat
/// root with 70,000 focusable children is cut at 65,536 nodes; the first children are kept in
/// order.
#[test]
fn a_tree_wider_than_the_node_limit_is_cut() {
    let mut world = World::new();
    let root = world.spawn((UiRoot::default(), UiNode)).id();
    let first = world.spawn((UiNode, Focusable::default(), ChildOf(root))).id();
    for _ in 1..70_000 {
        world.spawn((UiNode, Focusable::default(), ChildOf(root)));
    }
    let tree = build_a11y_tree(&mut world);
    assert_eq!(tree.nodes.len(), 65_536, "cut at the node limit");
    assert!(
        tree.nodes.iter().any(|n| n.id == first.to_bits()),
        "the walk keeps the first children"
    );
}

/// Audit wave 5 (accesskit H2, M5): the request queue a recording or stub backend fills is
/// bounded and drops the oldest requests; all requests drained in one frame are checked against
/// one tree and every kept one still runs, in order.
#[test]
fn the_request_queue_keeps_the_newest_requests_up_to_its_cap() {
    let (mut app, handle) = headless();
    let c = controls(app.world_mut());
    app.update();
    let extra = 10;
    for i in 0..regia_ui::a11y::MAX_QUEUED_REQUESTS + extra {
        handle.request(c.input.to_bits(), A11yAction::SetText(i.to_string()));
    }
    app.update();
    let results = outcome(&handle);
    assert_eq!(results.len(), regia_ui::a11y::MAX_QUEUED_REQUESTS);
    assert_eq!(results[0], (A11yAction::SetText(extra.to_string()), Ok(())));
    assert!(results.iter().all(|(_, r)| r.is_ok()));
    let last = regia_ui::a11y::MAX_QUEUED_REQUESTS + extra - 1;
    assert_eq!(
        app.world().get::<TextInput>(c.input).unwrap().text,
        last.to_string()
    );
}

/// Audit wave 5 (accesskit M4): a slider whose end is not finite exports no numeric range, and a
/// finite one still does.
#[test]
fn a_slider_with_a_non_finite_end_exports_no_range() {
    let mut world = World::new();
    let c = controls(&mut world);
    let finite = build_a11y_tree(&mut world);
    assert!(finite.get(c.slider.to_bits()).unwrap().range.is_some());
    world.get_mut::<Slider>(c.slider).unwrap().max = f32::NAN;
    let tree = build_a11y_tree(&mut world);
    assert_eq!(tree.get(c.slider.to_bits()).unwrap().range, None);
}

/// Audit wave 5 (accesskit M5): announcements queued in one frame are capped; the newest are the
/// ones kept, in order.
#[test]
fn announcements_queued_in_one_frame_keep_only_the_newest_up_to_the_cap() {
    let (mut app, handle) = headless();
    app.update();
    handle.clear_events();
    let cap = regia_ui::a11y::MAX_PENDING_ANNOUNCEMENTS;
    {
        let mut reader = app.world_mut().resource_mut::<ScreenReader>();
        for i in 0..cap + 36 {
            reader.announce(format!("n{i}"), regia_ui::a11y::Politeness::Polite);
        }
    }
    app.update();
    let said: Vec<String> = handle
        .events()
        .into_iter()
        .filter_map(|e| match e {
            A11yEvent::Announcement { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(said.len(), cap);
    assert_eq!(said.first().map(String::as_str), Some("n36"));
    assert_eq!(said.last(), Some(&format!("n{}", cap + 35)));
}

/// rc1 UI-text finding 5: the label part `sync_parts` spawns under a `TextInput` carries the
/// current content, so folding it into the accessible name spoke the content twice (name and
/// value) and re-announced the name on every keystroke. The parts are widget internals: the
/// placeholder names the field, the content is the value, and the parts are no tree nodes.
#[test]
fn text_input_name_comes_from_the_placeholder_not_the_content() {
    let (mut app, _handle) = headless();
    let c = controls(app.world_mut());
    app.update();
    app.world_mut()
        .get_mut::<TextInput>(c.input)
        .unwrap()
        .set_text("Ada");
    app.update();
    let world = app.world_mut();
    let label_part = world
        .get::<Children>(c.input)
        .unwrap()
        .iter()
        .find(|e| world.get::<regia_ui::TextInputPart>(*e) == Some(&regia_ui::TextInputPart::Label))
        .expect("the label part exists after the app ran");
    let tree = build_a11y_tree(world);
    let input = tree.get(c.input.to_bits()).expect("input node");
    assert_eq!(
        input.name, "Player name",
        "the placeholder names the field, never the content"
    );
    assert_eq!(input.value.as_deref(), Some("Ada"));
    assert!(
        tree.get(label_part.to_bits()).is_none(),
        "the label part is a widget internal, not a tree node"
    );
}
