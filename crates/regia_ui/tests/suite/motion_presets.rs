// SPDX-License-Identifier: MPL-2.0
//! Motion presets (UI default design D14, ADR-522): enter / exit / hover / press presets driven by
//! `run_ui_motion` through the tween system, and the reduced-motion setting from either source.
//! Headless: no window, no GPU, game time advances by a fixed 20 ms per update, and every
//! assertion counts updates or reads component values; nothing reads a wall clock.
//!
//! TODO(lead): a look check of the presets (a dialog sliding in, a card lifting on hover) needs a
//! GPU capture and a blessed golden; the tests below check the same values without pixels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{CursorMoved, MouseButtonInput};
use regia_ui::focus::UiInputMode;
use regia_ui::prelude::*;
use regia_ui::theme_resolve::builtin_theme_set;
use regia_ui::{
    ActiveTheme, ExitAction, Focused, Hovered, MotionPhase, MotionPose, PoseKind, Pressed, RevealPreset,
    TweenProperty, TweenValue, UiAccessibility, UiMotion, UiMotionState, UiPointer, UiTransform, UiTween,
};

const STEP_MS: u64 = 20;
/// More updates than any preset here needs (the longest is 320 ms = 16 steps).
const CAP: usize = 100;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<CursorMoved>()
        .add_message::<MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(STEP_MS)));
    app
}

fn computed(rect: [f32; 4]) -> ComputedNode {
    ComputedNode {
        rect,
        clip_rect: rect,
        scale: 1.0,
        ..Default::default()
    }
}

fn node(app: &mut App, motion: UiMotion) -> Entity {
    app.world_mut()
        .spawn((computed([100.0, 100.0, 80.0, 40.0]), NodeStyle::default(), motion))
        .id()
}

fn state(app: &App, e: Entity) -> UiMotionState {
    *app.world().get::<UiMotionState>(e).unwrap()
}

fn opacity(app: &App, e: Entity) -> f32 {
    app.world().get::<Opacity>(e).map_or(1.0, |o| o.0)
}

fn transform(app: &App, e: Entity) -> UiTransform {
    app.world().get::<UiTransform>(e).copied().unwrap_or_default()
}

fn display(app: &App, e: Entity) -> Display {
    app.world().get::<NodeStyle>(e).unwrap().0.display
}

/// Runs updates until the node's motion has settled (no tween, nothing running, nothing pending),
/// returning how many it took.
fn settle(app: &mut App, e: Entity) -> usize {
    for n in 1..=CAP {
        app.update();
        let Some(s) = app.world().get::<UiMotionState>(e) else {
            return n;
        };
        if app.world().get::<UiTween>(e).is_none() && s.running().is_none() && s.pending().is_none() {
            return n;
        }
    }
    panic!("motion did not settle within {CAP} updates");
}

fn request(app: &mut App, e: Entity, enter: bool) {
    let mut s = app.world_mut().get_mut::<UiMotionState>(e).unwrap();
    if enter {
        s.request_enter();
    } else {
        s.request_exit();
    }
}

#[test]
fn adding_a_dialog_slides_and_fades_it_in() {
    let mut app = app();
    let e = node(&mut app, UiMotion::preset("dialog").unwrap());
    app.update();
    assert_eq!(state(&app, e).phase, MotionPhase::Entering);
    assert!(app.world().get::<UiTween>(e).is_some());
    assert!(opacity(&app, e) < 1.0, "starts transparent");
    assert!(transform(&app, e).translate[1] > 0.0, "starts below");

    let mut last = opacity(&app, e);
    let mut updates = 1;
    while app.world().get::<UiTween>(e).is_some() {
        app.update();
        updates += 1;
        assert!(updates < CAP);
        let now = opacity(&app, e);
        assert!(now >= last, "the fade only rises: {last} -> {now}");
        last = now;
    }
    // motion.slow is 240 ms: 12 steps of 20 ms, plus at most two for a zero first delta and f32
    // rounding of the summed steps.
    assert!((12..=14).contains(&updates), "240 ms at 20 ms steps: {updates}");
    assert_eq!(opacity(&app, e), 1.0);
    assert_eq!(transform(&app, e).translate, [0.0, 0.0]);
    app.update();
    let s = state(&app, e);
    assert_eq!((s.phase, s.running()), (MotionPhase::Shown, None));
}

#[test]
fn exit_hides_at_its_end_and_enter_brings_the_node_back() {
    let mut app = app();
    let e = node(
        &mut app,
        UiMotion::preset("popup").unwrap().with_enter_on_add(false),
    );
    app.update();
    assert!(app.world().get::<UiTween>(e).is_none(), "no enter on add");

    request(&mut app, e, false);
    app.update();
    assert_eq!(state(&app, e).phase, MotionPhase::Exiting);
    assert_ne!(display(&app, e), Display::None, "still drawn while leaving");
    settle(&mut app, e);
    assert_eq!(state(&app, e).phase, MotionPhase::Hidden);
    assert_eq!(display(&app, e), Display::None);
    assert_eq!(opacity(&app, e), 0.0);
    assert!(
        (transform(&app, e).scale - 0.96).abs() < 1e-6,
        "zoomed to the hidden end"
    );

    request(&mut app, e, true);
    app.update();
    assert_ne!(display(&app, e), Display::None, "shown when the enter starts");
    settle(&mut app, e);
    assert_eq!(state(&app, e).phase, MotionPhase::Shown);
    assert_eq!(opacity(&app, e), 1.0);
    assert_eq!(transform(&app, e).scale, 1.0);
}

#[test]
fn a_toast_exit_despawns_the_node_and_its_children() {
    let mut app = app();
    let e = node(
        &mut app,
        UiMotion::preset("toast").unwrap().with_enter_on_add(false),
    );
    let child = app.world_mut().spawn((computed([0.0; 4]), ChildOf(e))).id();
    app.update();
    request(&mut app, e, false);
    let mut gone = None;
    for n in 1..=CAP {
        app.update();
        if app.world().get_entity(e).is_err() {
            gone = Some(n);
            break;
        }
        assert_eq!(state(&app, e).phase, MotionPhase::Exiting);
    }
    assert!(gone.is_some(), "the toast despawned");
    assert!(app.world().get_entity(child).is_err(), "with its subtree");
}

#[test]
fn an_exit_without_a_preset_hides_at_once() {
    let mut app = app();
    let e = node(&mut app, UiMotion::new());
    app.update();
    request(&mut app, e, false);
    app.update();
    assert_eq!(display(&app, e), Display::None);
    assert_eq!(state(&app, e).phase, MotionPhase::Hidden);
    assert!(app.world().get::<UiTween>(e).is_none());
    request(&mut app, e, true);
    app.update();
    assert_ne!(display(&app, e), Display::None);
    assert_eq!(state(&app, e).phase, MotionPhase::Shown);
}

#[test]
fn an_exit_during_the_enter_turns_round_where_the_node_is() {
    let mut app = app();
    let e = node(&mut app, UiMotion::preset("dialog").unwrap());
    for _ in 0..4 {
        app.update();
    }
    let mid = opacity(&app, e);
    assert!(mid > 0.0 && mid < 1.0, "mid-enter: {mid}");
    request(&mut app, e, false);
    app.update();
    let after = opacity(&app, e);
    assert!(after <= mid + 1e-6, "no jump to shown: {mid} -> {after}");
    assert!(after > 0.0, "no jump to hidden: {after}");
    assert_eq!(state(&app, e).phase, MotionPhase::Exiting);
    settle(&mut app, e);
    assert_eq!(state(&app, e).phase, MotionPhase::Hidden);
}

#[test]
fn hover_and_press_move_the_node_between_poses() {
    let mut app = app();
    let e = node(&mut app, UiMotion::preset("card").unwrap());
    settle(&mut app, e);
    assert!(
        app.world().get::<UiTransform>(e).is_none(),
        "rest needs no transform"
    );

    app.world_mut().entity_mut(e).insert(Hovered);
    app.update();
    assert_eq!(state(&app, e).pose, PoseKind::Hover);
    settle(&mut app, e);
    assert_eq!(transform(&app, e).translate, MotionPose::LIFT.translate);
    // The drawn rect moved; layout's rect is kept.
    let rect = app.world().get::<ComputedNode>(e).unwrap().rect;
    assert_eq!(rect, [100.0, 98.0, 80.0, 40.0]);

    app.world_mut().entity_mut(e).insert(Pressed);
    settle(&mut app, e);
    assert_eq!(state(&app, e).pose, PoseKind::Press);
    assert_eq!(transform(&app, e).translate, MotionPose::SINK.translate);

    app.world_mut().entity_mut(e).remove::<(Hovered, Pressed)>();
    settle(&mut app, e);
    assert_eq!(state(&app, e).pose, PoseKind::Rest);
    assert_eq!(transform(&app, e).translate, [0.0, 0.0]);
    assert_eq!(
        app.world().get::<ComputedNode>(e).unwrap().rect,
        [100.0, 100.0, 80.0, 40.0],
        "back on the layout rect"
    );
}

#[test]
fn the_real_pointer_drives_the_hover_pose() {
    let mut app = app();
    let e = app
        .world_mut()
        .spawn((
            computed([0.0, 0.0, 100.0, 40.0]),
            Interactable::default(),
            UiMotion::new().with_hover(MotionPose::GROW),
        ))
        .id();
    app.world_mut().write_message(CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (50.0, 20.0),
        timestamp: Default::default(),
    });
    for _ in 0..2 {
        app.update();
    }
    assert!(app.world().get::<Hovered>(e).is_some());
    settle(&mut app, e);
    let grown = app.world().get::<ComputedNode>(e).unwrap().rect;
    assert!((grown[2] - 104.0).abs() < 1e-3, "grew 4 percent: {grown:?}");

    app.world_mut().write_message(CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (500.0, 500.0),
        timestamp: Default::default(),
    });
    for _ in 0..2 {
        app.update();
    }
    assert!(app.world().get::<Hovered>(e).is_none());
    settle(&mut app, e);
    assert_eq!(
        app.world().get::<ComputedNode>(e).unwrap().rect,
        [0.0, 0.0, 100.0, 40.0]
    );
}

#[test]
fn disabled_and_touch_suppress_hover_and_navigation_focus_counts_as_hover() {
    let mut app = app();
    let disabled = node(&mut app, UiMotion::preset("card").unwrap());
    app.world_mut()
        .entity_mut(disabled)
        .insert((Hovered, Pressed, Disabled));
    let touched = node(&mut app, UiMotion::preset("card").unwrap());
    app.world_mut().entity_mut(touched).insert(Hovered);
    app.world_mut().resource_mut::<UiPointer>().active_touch = Some(7);
    for _ in 0..10 {
        app.update();
    }
    for e in [disabled, touched] {
        assert_eq!(state(&app, e).pose, PoseKind::Rest);
        assert!(app.world().get::<UiTween>(e).is_none());
        assert!(app.world().get::<UiTransform>(e).is_none());
    }

    let focused = node(&mut app, UiMotion::preset("card").unwrap());
    app.world_mut().entity_mut(focused).insert(Focused);
    settle(&mut app, focused);
    assert_eq!(
        state(&app, focused).pose,
        PoseKind::Rest,
        "pointer mode: no hover"
    );
    app.world_mut().insert_resource(UiInputMode::Navigation);
    settle(&mut app, focused);
    assert_eq!(state(&app, focused).pose, PoseKind::Hover);
    assert_eq!(transform(&app, focused).translate, MotionPose::LIFT.translate);
}

#[test]
fn a_foreign_tween_holds_the_pose_until_it_ends() {
    let mut app = app();
    let fade = regia_ui::Tween::to(TweenProperty::Opacity, TweenValue::Number(0.5), 0.2)
        .from(TweenValue::Number(1.0));
    let e = app
        .world_mut()
        .spawn((
            computed([0.0, 0.0, 10.0, 10.0]),
            UiMotion::preset("card").unwrap(),
            Hovered,
            UiTween::new(fade),
        ))
        .id();
    app.update();
    let running = app.world().get::<UiTween>(e).unwrap();
    assert_eq!(
        running.plan.properties(),
        vec![TweenProperty::Opacity],
        "the game's tween is not replaced"
    );
    assert_eq!(state(&app, e).pose, PoseKind::Rest);
    settle(&mut app, e);
    assert_eq!(opacity(&app, e), 0.5, "the game's tween finished");
    // The hover pose follows once the game's tween is done.
    settle(&mut app, e);
    assert_eq!(state(&app, e).pose, PoseKind::Hover);
    assert_eq!(transform(&app, e).translate, MotionPose::LIFT.translate);
}

#[test]
fn accessibility_reduced_motion_caps_the_fade_and_jumps_the_slide() {
    let mut app = app();
    app.world_mut().resource_mut::<UiAccessibility>().reduced_motion = true;
    let e = node(&mut app, UiMotion::preset("dialog").unwrap());
    let mut saw_partial_fade = false;
    let mut updates = 0;
    loop {
        app.update();
        updates += 1;
        assert!(updates < CAP);
        let Some(tween) = app.world().get::<UiTween>(e) else {
            break;
        };
        assert!(tween.follows_reduced_motion);
        if tween.elapsed > 0.0 {
            assert_eq!(transform(&app, e).translate, [0.0, 0.0], "the slide jumps");
        }
        let o = opacity(&app, e);
        saw_partial_fade |= o > 0.0 && o < 1.0;
    }
    assert!(saw_partial_fade, "the fade still plays");
    // motion.fast is 80 ms: 4 steps, plus at most two for a zero first delta and f32 rounding of
    // the summed steps (motion.slow would take 12).
    assert!(
        (4..=6).contains(&updates),
        "fade capped at motion.fast: {updates}"
    );
    assert_eq!(opacity(&app, e), 1.0);
}

#[test]
fn accessibility_reduced_motion_jumps_plain_tweens_and_poses() {
    let mut app = app();
    app.world_mut().resource_mut::<UiAccessibility>().reduced_motion = true;
    let plain = app
        .world_mut()
        .spawn((
            Opacity(0.0),
            UiTween::new(
                regia_ui::Tween::to(TweenProperty::Opacity, TweenValue::Number(1.0), 5.0)
                    .from(TweenValue::Number(0.0)),
            ),
        ))
        .id();
    let card = node(&mut app, UiMotion::preset("card").unwrap());
    app.world_mut().entity_mut(card).insert(Hovered);
    app.update();
    assert!(
        app.world().get::<UiTween>(plain).is_none(),
        "done after one update"
    );
    assert_eq!(opacity(&app, plain), 1.0);
    assert_eq!(
        transform(&app, card).translate,
        MotionPose::LIFT.translate,
        "the pose jumps"
    );
}

#[test]
fn theme_reduced_motion_still_works_for_presets() {
    let mut app = app();
    app.world_mut().resource_mut::<ActiveTheme>().reduced_motion = true;
    let e = node(&mut app, UiMotion::preset("popup").unwrap());
    let n = settle(&mut app, e);
    assert!(n <= 8, "reduced: {n} updates (motion.slow would take 12)");
    assert_eq!(transform(&app, e).scale, 1.0);
    assert_eq!(opacity(&app, e), 1.0);
}

#[test]
fn accessibility_reduced_motion_skips_the_theme_cross_fade() {
    let mut app = app();
    let e = app
        .world_mut()
        .spawn((UiNode, Classes(vec!["button".to_owned()])))
        .id();
    for _ in 0..12 {
        app.update();
    }
    app.world_mut().resource_mut::<UiAccessibility>().reduced_motion = true;
    app.world_mut().insert_resource(ActiveTheme::named("regia-light"));
    app.update();
    let light = builtin_theme_set()
        .resolve("regia-light", None)
        .unwrap()
        .color("color.surface-raised");
    assert_eq!(
        app.world().get::<Themed>(e).unwrap().background,
        light,
        "applies at once"
    );
}

#[test]
fn a_settled_node_stays_idle() {
    let mut app = app();
    let e = node(
        &mut app,
        UiMotion::preset("card")
            .unwrap()
            .with_reveal(RevealPreset::Fade)
            .with_exit_action(ExitAction::Hide),
    );
    settle(&mut app, e);
    for _ in 0..20 {
        app.update();
        assert!(app.world().get::<UiTween>(e).is_none());
    }
    let s = state(&app, e);
    assert_eq!(
        (s.phase, s.pose, s.running()),
        (MotionPhase::Shown, PoseKind::Rest, None)
    );
}
