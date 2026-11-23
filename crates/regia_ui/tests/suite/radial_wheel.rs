// SPDX-License-Identifier: MPL-2.0
//! Radial wheel end to end (DO27 ask 16): stick, mouse and touch sequences through the real
//! systems produce the right [`RadialEvent`]s, the wheel owns the controller while open, and its
//! chips follow the sector list. Headless: no window, no GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, GamepadAxis, GamepadAxisChanged, GamepadButton, GamepadButtonChanged};
use regia_ui::focus::UiNavActions;
use regia_ui::interaction::UiPointer;
use regia_ui::prelude::*;
use regia_ui::radial::{
    RadialChip, RadialConfirm, RadialEvent, RadialEventKind, RadialOpen, RadialSelection,
};
use regia_ui::widgets::VirtualButtons;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app
}

/// A wheel of `n` sectors with a fixed layout rect (400 px square at 100, 100).
fn wheel(app: &mut App, n: usize, configure: impl FnOnce(&mut RadialMenu)) -> Entity {
    let sectors = (0..n)
        .map(|i| RadialSector::new(format!("s{i}"), format!("Slot {i}")))
        .collect();
    let mut menu = RadialMenu::new(sectors).with_radii(60.0, 200.0);
    menu.animation_secs = 0.0;
    configure(&mut menu);
    app.world_mut()
        .spawn((
            menu,
            ComputedNode {
                rect: [100.0, 100.0, 400.0, 400.0],
                clip_rect: [100.0, 100.0, 400.0, 400.0],
                scale: 1.0,
                ..Default::default()
            },
        ))
        .id()
}

#[derive(Default)]
struct Log(Vec<RadialEventKind>);

fn update(app: &mut App, log: &mut Log) {
    app.update();
    log.0.extend(
        app.world_mut()
            .resource_mut::<Messages<RadialEvent>>()
            .iter_current_update_messages()
            .map(|e| e.kind.clone()),
    );
}

fn stick(app: &mut App, x: f32, y: f32) {
    for (axis, value) in [(GamepadAxis::RightStickX, x), (GamepadAxis::RightStickY, y)] {
        app.world_mut().write_message(GamepadAxisChanged {
            device: 1,
            axis,
            value,
        });
    }
}

fn menu(app: &App, e: Entity) -> &RadialMenu {
    app.world().get::<RadialMenu>(e).unwrap()
}

#[test]
fn right_stick_hold_to_open_release_to_select() {
    let mut app = app();
    let mut log = Log::default();
    let w = wheel(&mut app, 8, |_| {});
    app.world_mut().get_mut::<RadialMenu>(w).unwrap().set_held(true);
    update(&mut app, &mut log);
    assert!(menu(&app, w).open);
    // Push the stick right (sector 2 of 8), then slightly off-axis: no flicker.
    stick(&mut app, 1.0, 0.0);
    update(&mut app, &mut log);
    stick(&mut app, 1.0, 0.12);
    update(&mut app, &mut log);
    stick(&mut app, 0.95, -0.14);
    update(&mut app, &mut log);
    assert_eq!(menu(&app, w).selection, RadialSelection::Sector(2));
    app.world_mut().get_mut::<RadialMenu>(w).unwrap().set_held(false);
    update(&mut app, &mut log);
    assert_eq!(
        log.0,
        vec![
            RadialEventKind::Opened,
            RadialEventKind::Hover(RadialSelection::Sector(2)),
            RadialEventKind::Select {
                selection: RadialSelection::Sector(2),
                id: "s2".into()
            },
            RadialEventKind::Closed,
        ]
    );
    assert!(!menu(&app, w).open);
}

#[test]
fn stick_back_to_centre_then_release_cancels() {
    let mut app = app();
    let mut log = Log::default();
    let w = wheel(&mut app, 6, |_| {});
    app.world_mut().get_mut::<RadialMenu>(w).unwrap().set_held(true);
    update(&mut app, &mut log);
    stick(&mut app, 0.0, 1.0);
    update(&mut app, &mut log);
    stick(&mut app, 0.0, 0.0);
    update(&mut app, &mut log);
    app.world_mut().get_mut::<RadialMenu>(w).unwrap().set_held(false);
    update(&mut app, &mut log);
    assert!(
        log.0
            .contains(&RadialEventKind::Hover(RadialSelection::Sector(0)))
    );
    assert!(log.0.contains(&RadialEventKind::Hover(RadialSelection::None)));
    assert!(log.0.contains(&RadialEventKind::Cancel));
    assert!(!log.0.iter().any(|e| matches!(e, RadialEventKind::Select { .. })));
}

#[test]
fn open_action_from_virtual_buttons_drives_a_toggle_wheel() {
    let mut app = app();
    let mut log = Log::default();
    let w = wheel(&mut app, 4, |m| {
        m.open_mode = RadialOpen::Toggle;
        m.open_action = "Wheel".into();
    });
    let press = |app: &mut App, down: bool| {
        app.world_mut()
            .resource_mut::<VirtualButtons>()
            .0
            .insert("Wheel".into(), down);
    };
    press(&mut app, true);
    update(&mut app, &mut log);
    press(&mut app, false);
    update(&mut app, &mut log);
    assert!(
        menu(&app, w).open,
        "toggle stays open after the control is released"
    );
    press(&mut app, true);
    update(&mut app, &mut log);
    assert!(!menu(&app, w).open);
    assert_eq!(
        log.0,
        vec![
            RadialEventKind::Opened,
            RadialEventKind::Cancel,
            RadialEventKind::Closed
        ]
    );
}

#[test]
fn mouse_angle_selects_and_click_release_confirms() {
    let mut app = app();
    let mut log = Log::default();
    let w = wheel(&mut app, 4, |m| m.open_mode = RadialOpen::Toggle);
    app.world_mut().get_mut::<RadialMenu>(w).unwrap().open();
    app.world_mut().resource_mut::<UiPointer>().cursor = Some([300.0, 300.0]);
    update(&mut app, &mut log);
    update(&mut app, &mut log); // records the resting cursor
    // Move to the left of the centre (sector 3), outside the hub.
    app.world_mut().resource_mut::<UiPointer>().cursor = Some([150.0, 300.0]);
    update(&mut app, &mut log);
    assert_eq!(menu(&app, w).selection, RadialSelection::Sector(3));
    app.world_mut().resource_mut::<UiPointer>().primary_down = true;
    update(&mut app, &mut log);
    assert!(menu(&app, w).open, "release mode: press alone does not confirm");
    app.world_mut().resource_mut::<UiPointer>().primary_down = false;
    update(&mut app, &mut log);
    assert!(log.0.contains(&RadialEventKind::Select {
        selection: RadialSelection::Sector(3),
        id: "s3".into()
    }));
}

#[test]
fn mouse_over_the_hub_picks_the_centre_action() {
    let mut app = app();
    let mut log = Log::default();
    let w = wheel(&mut app, 4, |m| {
        m.center = Some(RadialSector::new("holster", "Holster"));
        m.confirm_mode = RadialConfirm::Press;
        m.open_mode = RadialOpen::Toggle;
    });
    app.world_mut().get_mut::<RadialMenu>(w).unwrap().open();
    app.world_mut().resource_mut::<UiPointer>().cursor = Some([300.0, 300.0]);
    update(&mut app, &mut log);
    update(&mut app, &mut log);
    app.world_mut().resource_mut::<UiPointer>().cursor = Some([310.0, 290.0]);
    update(&mut app, &mut log);
    assert_eq!(menu(&app, w).selection, RadialSelection::Center);
    app.world_mut().resource_mut::<UiPointer>().primary_down = true;
    update(&mut app, &mut log);
    assert!(log.0.contains(&RadialEventKind::Select {
        selection: RadialSelection::Center,
        id: "holster".into()
    }));
}

#[test]
fn touch_drag_from_the_press_point_selects_and_lift_confirms() {
    let mut app = app();
    let mut log = Log::default();
    let w = wheel(&mut app, 8, |m| m.open_mode = RadialOpen::Toggle);
    app.world_mut().get_mut::<RadialMenu>(w).unwrap().open();
    update(&mut app, &mut log);
    // A finger lands far from the wheel's centre and drags up-right by 90 px (sector 1 of 8).
    {
        let mut p = app.world_mut().resource_mut::<UiPointer>();
        p.touches.insert(7, [700.0, 700.0]);
        p.active_touch = Some(7);
    }
    update(&mut app, &mut log);
    app.world_mut()
        .resource_mut::<UiPointer>()
        .touches
        .insert(7, [770.0, 630.0]);
    update(&mut app, &mut log);
    assert_eq!(menu(&app, w).selection, RadialSelection::Sector(1));
    {
        let mut p = app.world_mut().resource_mut::<UiPointer>();
        p.touches.clear();
        p.active_touch = None;
    }
    update(&mut app, &mut log);
    assert!(log.0.contains(&RadialEventKind::Select {
        selection: RadialSelection::Sector(1),
        id: "s1".into()
    }));
    assert!(!menu(&app, w).open);
}

#[test]
fn a_tap_without_drag_cancels_the_touch_wheel() {
    let mut app = app();
    let mut log = Log::default();
    let w = wheel(&mut app, 8, |m| m.open_mode = RadialOpen::Toggle);
    app.world_mut().get_mut::<RadialMenu>(w).unwrap().open();
    update(&mut app, &mut log);
    {
        let mut p = app.world_mut().resource_mut::<UiPointer>();
        p.touches.insert(1, [500.0, 500.0]);
        p.active_touch = Some(1);
    }
    update(&mut app, &mut log);
    {
        let mut p = app.world_mut().resource_mut::<UiPointer>();
        p.touches.clear();
        p.active_touch = None;
    }
    update(&mut app, &mut log);
    assert!(log.0.contains(&RadialEventKind::Cancel));
}

#[test]
fn open_wheel_takes_nav_actions_and_submit_confirms() {
    let mut app = app();
    let mut log = Log::default();
    let w = wheel(&mut app, 4, |m| m.open_mode = RadialOpen::Toggle);
    app.world_mut().get_mut::<RadialMenu>(w).unwrap().open();
    update(&mut app, &mut log);
    // D-pad right steps the highlight; A confirms. Focus navigation sees neither.
    for button in [GamepadButton::DPadRight, GamepadButton::South] {
        app.world_mut().write_message(GamepadButtonChanged {
            device: 1,
            button,
            state: ButtonState::Pressed,
            value: 1.0,
        });
        update(&mut app, &mut log);
        assert!(!app.world().resource::<UiNavActions>().submit);
    }
    assert!(
        log.0
            .iter()
            .any(|e| matches!(e, RadialEventKind::Select { id, .. } if id == "s0"))
    );
}

#[test]
fn chips_follow_the_sector_list() {
    let mut app = app();
    let mut log = Log::default();
    let w = wheel(&mut app, 5, |m| m.center = Some(RadialSector::new("c", "Centre")));
    let chips = |app: &mut App| {
        app.world_mut()
            .query::<&RadialChip>()
            .iter(app.world())
            .filter(|c| c.menu == w)
            .count()
    };
    update(&mut app, &mut log);
    assert_eq!(chips(&mut app), 6);
    app.world_mut()
        .get_mut::<RadialMenu>(w)
        .unwrap()
        .sectors
        .push(RadialSector::new("extra", "Extra"));
    update(&mut app, &mut log);
    assert_eq!(chips(&mut app), 7);
}

#[derive(Component)]
struct ArchetypeMarker;

#[test]
fn shared_nav_actions_go_to_the_lowest_entity_wheel_regardless_of_archetype() {
    for marker_on_first in [false, true] {
        let mut app = app();
        let first = wheel(&mut app, 4, |m| m.open_mode = RadialOpen::Toggle);
        let second = wheel(&mut app, 4, |m| m.open_mode = RadialOpen::Toggle);
        let marked = if marker_on_first { first } else { second };
        app.world_mut().entity_mut(marked).insert(ArchetypeMarker);
        for w in [first, second] {
            app.world_mut().get_mut::<RadialMenu>(w).unwrap().open();
        }
        app.update();
        let mut selected = Vec::new();
        for button in [GamepadButton::DPadRight, GamepadButton::South] {
            app.world_mut().write_message(GamepadButtonChanged {
                device: 1,
                button,
                state: ButtonState::Pressed,
                value: 1.0,
            });
            app.update();
            selected.extend(
                app.world_mut()
                    .resource_mut::<Messages<RadialEvent>>()
                    .iter_current_update_messages()
                    .filter(|e| matches!(e.kind, RadialEventKind::Select { .. }))
                    .map(|e| e.target),
            );
        }
        // The lowest by `Entity`'s order, which is not spawn order in bevy_ecs 0.20.
        assert_eq!(
            selected,
            vec![first.min(second)],
            "marker_on_first = {marker_on_first}"
        );
    }
}
