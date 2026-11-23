// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): layer-stack errors, drop-target filtering, parent walks and the
//! clipboard seam: the small API seams the larger tests only touch on their happy paths.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use regia_app::prelude::*;
use regia_ui::clipboard::{ClipboardProvider, MemoryClipboard, UiClipboard};
use regia_ui::dragdrop::{DragCancelReason, DragDrop, DropTarget, cancel_drag, nearest_target};
use regia_ui::layers::{InputLayer, InputLayers, LayerError, LayerInputMode, pop_layer, push_layer};

#[test]
fn layer_mode_names_round_trip_and_unknown_names_are_none() {
    for mode in [
        LayerInputMode::UiOnly,
        LayerInputMode::UiAndGame,
        LayerInputMode::GameOnly,
    ] {
        assert_eq!(LayerInputMode::parse(mode.name()), Some(mode));
    }
    for bad in ["", "UiOnly", "ui-only", "ui_only ", "none"] {
        assert_eq!(LayerInputMode::parse(bad), None, "{bad:?}");
    }
    assert_eq!(LayerInputMode::default(), LayerInputMode::UiAndGame);
}

#[test]
fn layer_error_codes_are_protocol_safe() {
    for e in [
        LayerError::UnknownEntity,
        LayerError::AlreadyLayer,
        LayerError::NotALayer,
        LayerError::NoLayers,
    ] {
        let code = e.code();
        assert!(
            !code.is_empty()
                && code
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{code}"
        );
    }
}

#[test]
fn layer_operations_report_errors_and_leave_the_stack_alone() {
    let mut world = World::new();
    world.init_resource::<InputLayers>();
    assert_eq!(pop_layer(&mut world, None), Err(LayerError::NoLayers));
    let plain = world.spawn_empty().id();
    assert_eq!(pop_layer(&mut world, Some(plain)), Err(LayerError::NotALayer));
    let gone = world.spawn_empty().id();
    world.despawn(gone);
    assert_eq!(
        push_layer(&mut world, gone, LayerInputMode::UiOnly),
        Err(LayerError::UnknownEntity)
    );
    assert_eq!(pop_layer(&mut world, Some(gone)), Err(LayerError::UnknownEntity));

    push_layer(&mut world, plain, LayerInputMode::UiOnly).expect("push");
    assert_eq!(
        push_layer(&mut world, plain, LayerInputMode::GameOnly),
        Err(LayerError::AlreadyLayer)
    );
    let layers = world.resource::<InputLayers>();
    assert_eq!(layers.depth(), 1);
    assert_eq!(
        layers.mode(),
        LayerInputMode::UiOnly,
        "a refused push changed nothing"
    );
    assert_eq!(pop_layer(&mut world, None), Ok(plain));
    assert!(world.get::<InputLayer>(plain).is_none());
    let layers = world.resource::<InputLayers>();
    assert_eq!(
        (layers.depth(), layers.mode(), layers.top_root()),
        (0, LayerInputMode::UiAndGame, None)
    );
}

#[test]
fn the_effective_mode_is_the_top_layers_and_popping_restores_the_one_below() {
    let mut world = World::new();
    world.init_resource::<InputLayers>();
    let (a, b, c) = (
        world.spawn_empty().id(),
        world.spawn_empty().id(),
        world.spawn_empty().id(),
    );
    push_layer(&mut world, a, LayerInputMode::UiOnly).unwrap();
    push_layer(&mut world, b, LayerInputMode::GameOnly).unwrap();
    push_layer(&mut world, c, LayerInputMode::UiAndGame).unwrap();
    assert_eq!(world.resource::<InputLayers>().mode(), LayerInputMode::UiAndGame);
    // Popping a layer from the middle keeps the rest in order.
    assert_eq!(pop_layer(&mut world, Some(b)), Ok(b));
    let roots: Vec<Entity> = world
        .resource::<InputLayers>()
        .entries()
        .iter()
        .map(|e| e.root)
        .collect();
    assert_eq!(roots, [a, c]);
    assert_eq!(pop_layer(&mut world, None), Ok(c));
    assert_eq!(world.resource::<InputLayers>().mode(), LayerInputMode::UiOnly);
}

#[test]
fn drop_targets_filter_by_kind() {
    let any = DropTarget::default();
    assert!(any.accepts_kind("item") && any.accepts_kind(""));
    let only = DropTarget::accepting(["item", "tab"]);
    assert!(only.accepts_kind("tab"));
    assert!(!only.accepts_kind("Item"), "kinds are case sensitive");
    assert!(!only.accepts_kind(""));
    assert!(!only.accepts_kind("item "));
    let empty_kind = DropTarget::accepting([""]);
    assert!(empty_kind.accepts_kind("") && !empty_kind.accepts_kind("x"));
}

#[test]
fn nearest_target_walks_up_and_is_bounded() {
    let id = |n: u32| Entity::from_bits(u64::from(n));
    let parents: HashMap<Entity, Entity> = [(id(3), id(2)), (id(2), id(1))].into();
    let walk = |start: Entity, target: Option<Entity>| {
        nearest_target(start, |e| parents.get(&e).copied(), |e| Some(e) == target)
    };
    assert_eq!(walk(id(3), Some(id(3))), Some(id(3)), "the start itself");
    assert_eq!(walk(id(3), Some(id(1))), Some(id(1)), "an ancestor");
    assert_eq!(walk(id(3), Some(id(9))), None, "no target above");
    assert_eq!(walk(id(1), None), None, "a root with no parent");
    // A cycle in bad data terminates instead of hanging.
    let looped: HashMap<Entity, Entity> = [(id(1), id(2)), (id(2), id(1))].into();
    assert_eq!(
        nearest_target(id(1), |e| looped.get(&e).copied(), |_| false),
        None
    );
}

#[test]
fn cancel_drag_only_acts_on_a_running_operation() {
    let mut state = DragDrop::default();
    assert!(!cancel_drag(&mut state), "nothing to cancel");
    assert!(state.active().is_none());
    assert_eq!(DragCancelReason::Requested.name(), "requested");
    assert_eq!(DragCancelReason::NoTarget.name(), "no_target");
}

#[test]
fn clipboard_keeps_text_exactly_and_overwrites() {
    let probe = MemoryClipboard::default();
    let mut clip = UiClipboard::new(probe.clone());
    assert_eq!(clip.get_text(), None);
    for text in ["", "line1\nline2\r\n", "tab\there", "日本語 😀", "\0nul"] {
        clip.set_text(text);
        assert_eq!(clip.get_text().as_deref(), Some(text));
        assert_eq!(probe.contents().as_deref(), Some(text));
    }
    // An empty string is a value, not "no text".
    clip.set_text("");
    assert_eq!(clip.get_text().as_deref(), Some(""));
    // A provider handed straight to the trait behaves the same.
    let mut raw = MemoryClipboard::default();
    raw.set_text("a");
    raw.set_text("b");
    assert_eq!(ClipboardProvider::get_text(&mut raw).as_deref(), Some("b"));
}

#[test]
fn modifier_keys_update_the_held_state_by_name() {
    use regia_ui::interaction::UiModifiers;
    let mut m = UiModifiers::default();
    assert!(m.is_empty());
    for (key, held) in [
        ("Shift", "shift"),
        ("CONTROL", "ctrl"),
        ("ctrl", "ctrl"),
        ("AltGraph", "alt"),
        ("Option", "alt"),
        ("Super", "meta"),
        ("OS", "meta"),
        ("Command", "meta"),
    ] {
        let mut m = UiModifiers::default();
        assert!(m.apply_key(key, true), "{key} is a modifier");
        let flags = [
            ("shift", m.shift),
            ("ctrl", m.ctrl),
            ("alt", m.alt),
            ("meta", m.meta),
        ];
        assert!(
            flags.iter().all(|(name, on)| *on == (*name == held)),
            "{key}: {flags:?}"
        );
        assert!(m.apply_key(key, false), "{key} released");
        assert!(m.is_empty(), "{key}");
    }
    for key in ["a", "Enter", "", "Shiftx", "Ctrl+", " shift"] {
        assert!(!m.apply_key(key, true), "{key:?} is not a modifier");
        assert!(m.is_empty(), "a non-modifier key changes nothing");
    }
    // Releasing a key that was never pressed stays released; two keys held at once both show.
    assert!(m.apply_key("shift", false) && m.is_empty());
    m.apply_key("shift", true);
    m.apply_key("ctrl", true);
    assert!(m.shift && m.ctrl && !m.alt && !m.meta && !m.is_empty());
}
