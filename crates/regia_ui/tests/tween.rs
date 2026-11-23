// SPDX-License-Identifier: MPL-2.0
//! Property tweens and the layout-neutral transform (ADR-273 lane 3, ADR-343). Headless: no window,
//! no GPU, game time advances by a fixed 100 ms per update, and every assertion counts updates or
//! reads the tween's own `elapsed`; nothing reads a wall clock.
//!
//! TODO(lead): the roadmap also asks for a fade-in screenshot at t = 0, 0.5 and 1 s within
//! tolerance. That needs a GPU reference and a blessed golden, which this lane cannot make; the
//! tests below check the same values (opacity and rects at those times) without pixels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{CursorMoved, MouseButtonInput};
use regia_ui::prelude::*;
use regia_ui::{
    ActiveTheme, BackgroundKind, Ease, HitTestSpace, Hovered, MotionPose, Reveal, Tween, TweenPlan,
    TweenProperty, TweenValue, UiEvent, UiEventKind, UiLayoutBox, UiMotion, UiTransform, UiTween,
    UiTweenFinished,
};

const STEP_MS: u64 = 100;

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

fn number(property: TweenProperty, from: f32, to: f32, seconds: f32) -> Tween {
    Tween::to(property, TweenValue::Number(to), seconds).from(TweenValue::Number(from))
}

fn finished_this_update(app: &App) -> Vec<Entity> {
    app.world()
        .resource::<Messages<UiTweenFinished>>()
        .iter_current_update_messages()
        .map(|m| m.entity)
        .collect()
}

/// Runs updates until `entity` has no tween, returning how many it took (capped, no clock).
fn run_until_idle(app: &mut App, entity: Entity) -> usize {
    for n in 1..=200 {
        app.update();
        if app.world().get::<UiTween>(entity).is_none() {
            return n;
        }
    }
    panic!("tween did not finish within 200 updates");
}

#[test]
fn opacity_follows_the_pure_sample_and_finishes_once() {
    let mut app = app();
    let plan: TweenPlan = number(TweenProperty::Opacity, 0.0, 1.0, 1.0).into();
    let e = app
        .world_mut()
        .spawn((Opacity(0.0), UiTween::new(plan.clone())))
        .id();

    let mut last = -1.0f32;
    let mut finishes = 0;
    for _ in 0..200 {
        app.update();
        finishes += finished_this_update(&app).iter().filter(|f| **f == e).count();
        let opacity = app.world().get::<Opacity>(e).unwrap().0;
        let sampled = match app.world().get::<UiTween>(e) {
            Some(t) => {
                let mut out = Vec::new();
                plan.sample(t.elapsed, &mut out);
                match out.last() {
                    Some((TweenProperty::Opacity, TweenValue::Number(v))) => *v,
                    other => panic!("unexpected sample {other:?}"),
                }
            }
            None => 1.0,
        };
        assert!(
            (opacity - sampled).abs() < 1e-5,
            "opacity {opacity} vs sample {sampled}"
        );
        assert!(opacity >= last, "linear fade only rises");
        last = opacity;
        if app.world().get::<UiTween>(e).is_none() {
            break;
        }
    }
    assert_eq!(last, 1.0, "ends at the end value");
    assert_eq!(finishes, 1, "one completion message");
    assert!(
        app.world().get::<UiTween>(e).is_none(),
        "the tween removes itself"
    );
}

#[test]
fn two_runs_at_the_same_step_agree_exactly() {
    let run = || {
        let mut app = app();
        let plan = TweenPlan::Parallel(vec![
            number(TweenProperty::Opacity, 0.0, 1.0, 0.7)
                .ease(Ease::ElasticOut)
                .into(),
            number(TweenProperty::Width, 10.0, 90.0, 0.7)
                .ease(Ease::BounceOut)
                .into(),
        ]);
        let e = app
            .world_mut()
            .spawn((Opacity(0.0), NodeStyle::default(), UiTween::new(plan)))
            .id();
        let mut trace = Vec::new();
        for _ in 0..10 {
            app.update();
            let w = match app.world().get::<NodeStyle>(e).unwrap().0.size[0] {
                Dimension::Points(w) => w,
                other => panic!("width {other:?}"),
            };
            trace.push((app.world().get::<Opacity>(e).unwrap().0.to_bits(), w.to_bits()));
        }
        trace
    };
    assert_eq!(run(), run());
}

#[test]
fn a_paused_game_clock_freezes_tweens() {
    let mut app = app();
    let e = app
        .world_mut()
        .spawn((
            Opacity(0.0),
            UiTween::new(number(TweenProperty::Opacity, 0.0, 1.0, 1.0)),
        ))
        .id();
    app.update();
    app.update();
    app.world_mut().resource_mut::<Time>().paused = true;
    app.update();
    let frozen = app.world().get::<UiTween>(e).unwrap().elapsed;
    let value = app.world().get::<Opacity>(e).unwrap().0;
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(app.world().get::<UiTween>(e).unwrap().elapsed, frozen);
    assert_eq!(app.world().get::<Opacity>(e).unwrap().0, value);
    app.world_mut().resource_mut::<Time>().paused = false;
    run_until_idle(&mut app, e);
    assert_eq!(app.world().get::<Opacity>(e).unwrap().0, 1.0);
}

#[test]
fn open_starts_take_the_current_value_and_missing_components_are_added() {
    let mut app = app();
    let style = NodeStyle::new(Style {
        size: [Dimension::Points(100.0), Dimension::Points(40.0)],
        ..Default::default()
    });
    let plan = TweenPlan::Parallel(vec![
        Tween::to(TweenProperty::Width, TweenValue::Number(200.0), 1.0).into(),
        Tween::to(TweenProperty::Background, TweenValue::Color(UiColor::WHITE), 1.0).into(),
    ]);
    let e = app.world_mut().spawn((style, UiTween::new(plan))).id();
    app.update();
    let width = match app.world().get::<NodeStyle>(e).unwrap().0.size[0] {
        Dimension::Points(w) => w,
        other => panic!("width {other:?}"),
    };
    assert!((100.0..200.0).contains(&width), "starts at 100: {width}");
    app.update();
    let bg = app.world().get::<Background>(e).expect("background added");
    assert!(matches!(bg.kind, BackgroundKind::Color(_)));
    run_until_idle(&mut app, e);
    assert_eq!(
        app.world().get::<NodeStyle>(e).unwrap().0.size[0],
        Dimension::Points(200.0)
    );
    assert_eq!(
        app.world().get::<NodeStyle>(e).unwrap().0.size[1],
        Dimension::Points(40.0)
    );
    assert_eq!(
        app.world().get::<Background>(e).unwrap().kind,
        BackgroundKind::Color(UiColor::WHITE)
    );
}

#[test]
fn a_sequence_runs_one_step_after_another() {
    let mut app = app();
    let plan = TweenPlan::Sequence(vec![
        number(TweenProperty::Opacity, 0.0, 1.0, 0.5).into(),
        number(TweenProperty::Opacity, 1.0, 0.25, 0.5).into(),
    ]);
    let e = app.world_mut().spawn((Opacity(0.0), UiTween::new(plan))).id();
    let updates = run_until_idle(&mut app, e);
    assert!((10..=12).contains(&updates), "1 s at 100 ms steps: {updates}");
    assert_eq!(app.world().get::<Opacity>(e).unwrap().0, 0.25);
}

#[test]
fn a_settled_hover_drops_an_identity_transform() {
    let mut app = app();
    let e = app
        .world_mut()
        .spawn((
            computed([100.0, 100.0, 80.0, 40.0]),
            NodeStyle::default(),
            Interactable::default(),
            UiMotion::new().with_hover(MotionPose::LIFT),
        ))
        .id();
    // A few hundred unrelated nodes: an identity transform must not scan them on an idle frame.
    for _ in 0..300 {
        app.world_mut().spawn(computed([0.0, 0.0, 4.0, 4.0]));
    }
    app.world_mut().entity_mut(e).insert(Hovered);
    let _ = run_until_idle(&mut app, e);
    assert!(app.world().get::<UiTween>(e).is_none());
    assert!(
        app.world().get::<UiTransform>(e).is_some(),
        "the hover pose is not identity"
    );
    app.world_mut().entity_mut(e).remove::<Hovered>();
    let _ = run_until_idle(&mut app, e);
    assert!(
        app.world().get::<UiTween>(e).is_none(),
        "the return tween finished"
    );
    assert!(
        app.world().get::<UiTransform>(e).is_none(),
        "an identity pose still carries UiTransform"
    );
    let before = app
        .world()
        .resource::<regia_ui::UiHotPathCounts>()
        .transform_node_scans;
    app.update();
    let after = app
        .world()
        .resource::<regia_ui::UiHotPathCounts>()
        .transform_node_scans;
    assert_eq!(after, before, "an idle frame scanned every ComputedNode");
}

#[test]
fn reduced_motion_jumps_to_the_end_value() {
    let mut app = app();
    app.world_mut().resource_mut::<ActiveTheme>().reduced_motion = true;
    let e = app
        .world_mut()
        .spawn((
            Opacity(0.0),
            UiTween::new(number(TweenProperty::Opacity, 0.0, 1.0, 5.0)),
        ))
        .id();
    app.update();
    assert!(app.world().get::<UiTween>(e).is_none(), "done after one update");
    assert_eq!(app.world().get::<Opacity>(e).unwrap().0, 1.0);
    assert_eq!(finished_this_update(&app), vec![e]);
}

#[test]
fn show_and_hide_toggle_display() {
    let mut app = app();
    let hidden = NodeStyle::new(Style {
        display: Display::None,
        ..Default::default()
    });
    let e = app.world_mut().spawn(hidden).id();
    app.world_mut()
        .entity_mut(e)
        .insert(UiTween::show(Reveal::Slide([0.0, 30.0]), 0.3, Ease::QuadOut));
    app.update();
    assert_ne!(app.world().get::<NodeStyle>(e).unwrap().0.display, Display::None);
    run_until_idle(&mut app, e);
    assert_eq!(app.world().get::<Opacity>(e).unwrap().0, 1.0);
    let t = app
        .world()
        .get::<UiTransform>(e)
        .copied()
        .unwrap_or(UiTransform::IDENTITY);
    assert_eq!(t.translate, [0.0, 0.0]);

    app.world_mut()
        .entity_mut(e)
        .insert(UiTween::hide(Reveal::Fade, 0.3, Ease::Linear));
    run_until_idle(&mut app, e);
    assert_eq!(app.world().get::<Opacity>(e).unwrap().0, 0.0);
    assert_eq!(app.world().get::<NodeStyle>(e).unwrap().0.display, Display::None);
}

#[test]
fn text_colour_lands_on_every_span_at_the_end() {
    let mut app = app();
    let target = UiColor([0.1, 0.8, 0.2, 1.0]);
    let e = app
        .world_mut()
        .spawn((
            Text::new("Ready"),
            UiTween::new(Tween::to(
                TweenProperty::TextColor,
                TweenValue::Color(target),
                0.3,
            )),
        ))
        .id();
    run_until_idle(&mut app, e);
    let text = app.world().get::<Text>(e).unwrap();
    assert!(text.spans.iter().all(|s| s.style.color == target));
}

#[test]
fn replacing_a_tween_does_not_send_a_completion() {
    let mut app = app();
    let e = app
        .world_mut()
        .spawn((
            Opacity(0.0),
            UiTween::new(number(TweenProperty::Opacity, 0.0, 1.0, 10.0)),
        ))
        .id();
    app.update();
    app.world_mut()
        .entity_mut(e)
        .insert(UiTween::new(number(TweenProperty::Opacity, 0.5, 1.0, 10.0)));
    app.update();
    assert!(finished_this_update(&app).is_empty());
    assert!(app.world().get::<UiTween>(e).is_some());
    app.world_mut().entity_mut(e).remove::<UiTween>();
    app.update();
    assert!(finished_this_update(&app).is_empty(), "a stop is not a finish");
}

// ---------------------------------------------------------------------------------------------
// Transform
// ---------------------------------------------------------------------------------------------

fn panel_with_child(app: &mut App) -> (Entity, Entity) {
    let parent = app
        .world_mut()
        .spawn((computed([100.0, 100.0, 100.0, 50.0]), Interactable::default()))
        .id();
    let child = app
        .world_mut()
        .spawn((computed([110.0, 110.0, 20.0, 10.0]), ChildOf(parent)))
        .id();
    (parent, child)
}

fn rect(app: &App, e: Entity) -> [f32; 4] {
    app.world().get::<ComputedNode>(e).unwrap().rect
}

#[test]
fn scale_about_the_centre_maps_the_node_and_its_subtree() {
    let mut app = app();
    let (parent, child) = panel_with_child(&mut app);
    app.update();
    app.world_mut().entity_mut(parent).insert(UiTransform {
        scale: 2.0,
        ..UiTransform::default()
    });
    app.update();
    // Pivot (150, 125): the parent doubles about it.
    assert_eq!(rect(&app, parent), [50.0, 75.0, 200.0, 100.0]);
    // The child follows: (110 - 150) * 2 + 150 = 70.
    assert_eq!(rect(&app, child), [70.0, 95.0, 40.0, 20.0]);
    let b = app.world().get::<UiLayoutBox>(parent).expect("layout rect kept");
    assert_eq!(b.base.rect, [100.0, 100.0, 100.0, 50.0]);
    assert!(app.world().get::<UiLayoutBox>(child).is_some());

    // Steady frames do not compound the transform.
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(rect(&app, parent), [50.0, 75.0, 200.0, 100.0]);
    assert_eq!(rect(&app, child), [70.0, 95.0, 40.0, 20.0]);

    // Removing it gives layout's rects back.
    app.world_mut().entity_mut(parent).remove::<UiTransform>();
    app.update();
    assert_eq!(rect(&app, parent), [100.0, 100.0, 100.0, 50.0]);
    assert_eq!(rect(&app, child), [110.0, 110.0, 20.0, 10.0]);
    assert!(app.world().get::<UiLayoutBox>(parent).is_none());
    assert!(app.world().get::<UiLayoutBox>(child).is_none());
}

#[test]
fn translate_moves_without_touching_the_layout_rect() {
    let mut app = app();
    let (parent, child) = panel_with_child(&mut app);
    app.world_mut().entity_mut(parent).insert(UiTransform {
        translate: [30.0, -10.0],
        ..UiTransform::default()
    });
    app.update();
    assert_eq!(rect(&app, parent), [130.0, 90.0, 100.0, 50.0]);
    assert_eq!(rect(&app, child), [140.0, 100.0, 20.0, 10.0]);
    assert_eq!(
        app.world().get::<UiLayoutBox>(parent).unwrap().base.rect,
        [100.0, 100.0, 100.0, 50.0]
    );
}

#[test]
fn a_fresh_layout_result_becomes_the_new_base() {
    let mut app = app();
    let (parent, _child) = panel_with_child(&mut app);
    app.world_mut().entity_mut(parent).insert(UiTransform {
        translate: [10.0, 0.0],
        ..UiTransform::default()
    });
    app.update();
    assert_eq!(rect(&app, parent), [110.0, 100.0, 100.0, 50.0]);
    // What a relayout does: write layout's rect over the transformed one.
    *app.world_mut().get_mut::<ComputedNode>(parent).unwrap() = computed([100.0, 200.0, 100.0, 50.0]);
    app.update();
    assert_eq!(rect(&app, parent), [110.0, 200.0, 100.0, 50.0]);
    assert_eq!(
        app.world().get::<UiLayoutBox>(parent).unwrap().base.rect,
        [100.0, 200.0, 100.0, 50.0]
    );
}

#[test]
fn nested_transforms_compose_inner_first() {
    let mut app = app();
    let (parent, child) = panel_with_child(&mut app);
    app.world_mut().entity_mut(parent).insert(UiTransform {
        translate: [100.0, 0.0],
        ..UiTransform::default()
    });
    app.world_mut().entity_mut(child).insert(UiTransform {
        scale: 2.0,
        pivot_ratio: [0.0, 0.0],
        ..UiTransform::default()
    });
    app.update();
    // Child: scale 2 about its top-left in layout space, then the parent's 100 px shift.
    assert_eq!(rect(&app, child), [210.0, 110.0, 40.0, 20.0]);
    assert_eq!(rect(&app, parent), [200.0, 100.0, 100.0, 50.0]);
}

#[test]
fn an_identity_transform_leaves_nothing_behind() {
    let mut app = app();
    let (parent, child) = panel_with_child(&mut app);
    app.world_mut().entity_mut(parent).insert(UiTransform::default());
    app.update();
    assert_eq!(rect(&app, parent), [100.0, 100.0, 100.0, 50.0]);
    assert!(app.world().get::<UiLayoutBox>(parent).is_none());
    assert!(app.world().get::<UiLayoutBox>(child).is_none());
}

#[test]
fn a_scale_tween_pops_a_hovered_button_and_settles_back() {
    let mut app = app();
    let button = app
        .world_mut()
        .spawn((computed([0.0, 0.0, 100.0, 40.0]), Interactable::default()))
        .id();
    // Game code reacting to hover: the whole "pop" is two tweens.
    app.add_systems(
        Update,
        move |mut events: MessageReader<UiEvent>, mut commands: Commands| {
            for e in events.read() {
                let (from, to) = match e.kind {
                    UiEventKind::PointerEnter => (1.0, 1.1),
                    UiEventKind::PointerLeave => (1.1, 1.0),
                    _ => continue,
                };
                commands.entity(e.target).insert(UiTween::new(
                    number(TweenProperty::Scale, from, to, 0.3).ease(Ease::BackOut),
                ));
            }
        },
    );
    app.world_mut().write_message(CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (50.0, 20.0),
        timestamp: Default::default(),
    });
    for _ in 0..8 {
        app.update();
    }
    let hovered = rect(&app, button);
    assert!((hovered[2] - 110.0).abs() < 1e-3, "grew 10 percent: {hovered:?}");
    assert!((hovered[0] + 5.0).abs() < 1e-3, "about the centre: {hovered:?}");

    app.world_mut().write_message(CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (500.0, 500.0),
        timestamp: Default::default(),
    });
    for _ in 0..8 {
        app.update();
    }
    assert_eq!(rect(&app, button), [0.0, 0.0, 100.0, 40.0], "back to layout");
    assert!(app.world().get::<UiLayoutBox>(button).is_none());
}

fn hover_target(app: &mut App, x: f64, y: f64) -> Option<Entity> {
    app.world_mut().write_message(CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (x, y),
        timestamp: Default::default(),
    });
    app.update();
    app.world()
        .resource::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .find(|e| e.kind == UiEventKind::PointerEnter)
        .map(|e| e.target)
}

#[test]
fn hit_testing_follows_the_transformed_rect_by_default() {
    let mut app = app();
    let button = app
        .world_mut()
        .spawn((
            computed([100.0, 100.0, 100.0, 100.0]),
            Interactable::default(),
            UiTransform {
                translate: [300.0, 0.0],
                ..UiTransform::default()
            },
        ))
        .id();
    app.update();
    assert_eq!(hover_target(&mut app, 150.0, 150.0), None, "layout spot is empty");
    assert_eq!(
        hover_target(&mut app, 450.0, 150.0),
        Some(button),
        "drawn spot hits"
    );
}

#[test]
fn the_layout_hit_test_flag_keeps_the_old_target() {
    let mut app = app();
    let button = app
        .world_mut()
        .spawn((
            computed([100.0, 100.0, 100.0, 100.0]),
            Interactable::default(),
            UiTransform {
                translate: [300.0, 0.0],
                hit_test: HitTestSpace::Layout,
                ..UiTransform::default()
            },
        ))
        .id();
    app.update();
    assert_eq!(
        hover_target(&mut app, 450.0, 150.0),
        None,
        "drawn spot does not hit"
    );
    assert_eq!(
        hover_target(&mut app, 150.0, 150.0),
        Some(button),
        "layout spot hits"
    );
}

/// Review rev-kimi-20 finding 3: `UiHotPathCounts::transform_node_scans` counts the idle frames
/// that look at every `ComputedNode` because some `UiTransform` exists, so
/// `a_settled_hover_drops_an_identity_transform` can see the scan it says it removes. A transform
/// that stays (a settled non-identity pose) scans on each idle frame; once it is gone, idle
/// frames stop scanning.
#[test]
fn a_live_transform_scans_on_idle_frames_and_its_removal_stops_the_scan() {
    let mut app = app();
    let e = app
        .world_mut()
        .spawn((
            computed([100.0, 100.0, 80.0, 40.0]),
            UiTransform {
                translate: [4.0, 0.0],
                ..UiTransform::IDENTITY
            },
        ))
        .id();
    for _ in 0..50 {
        app.world_mut().spawn(computed([0.0, 0.0, 4.0, 4.0]));
    }
    // The first frames apply the transform; after that nothing changes.
    for _ in 0..3 {
        app.update();
    }
    let scans = |app: &App| {
        app.world()
            .resource::<regia_ui::UiHotPathCounts>()
            .transform_node_scans
    };
    let before = scans(&app);
    const FRAMES: u64 = 4;
    for _ in 0..FRAMES {
        app.update();
    }
    assert_eq!(
        scans(&app) - before,
        FRAMES,
        "each idle frame with a live transform scans the nodes once"
    );
    app.world_mut().entity_mut(e).remove::<UiTransform>();
    app.update();
    let before = scans(&app);
    for _ in 0..FRAMES {
        app.update();
    }
    assert_eq!(scans(&app), before, "no transform left, no scan");
}

// ---------------------------------------------------------------------------------------------
// Frame events (`.uianim.ron` v2, ADR-8900)
// ---------------------------------------------------------------------------------------------

fn fired_this_update(app: &App) -> Vec<(Entity, String, String)> {
    app.world()
        .resource::<Messages<regia_ui::UiAnimEventFired>>()
        .iter_current_update_messages()
        .map(|m| (m.entity, m.name.clone(), m.payload.clone()))
        .collect()
}

fn anim_with_events(events: &[(f32, &str)]) -> regia_ui::anim_timeline::UiAnimDef {
    use regia_ui::anim_timeline::{AnimEvent, AnimKey, AnimTrack, AnimValue, LoopMode, UiAnimDef};
    UiAnimDef {
        tracks: vec![AnimTrack {
            property: "opacity".into(),
            keys: vec![
                AnimKey {
                    time: 0.0,
                    value: AnimValue::Number(0.0),
                    ease: None,
                },
                AnimKey {
                    time: 1.0,
                    value: AnimValue::Number(1.0),
                    ease: None,
                },
            ],
        }],
        mode: LoopMode::Once,
        loops: 1,
        speed: 1.0,
        events: events
            .iter()
            .map(|(time, name)| AnimEvent {
                time: *time,
                name: (*name).into(),
                payload: format!("p-{name}"),
            })
            .collect(),
    }
}

/// Every event fires exactly once, in the update whose step crosses it (the tween's elapsed time moves from
/// below the event's time to at or past it in that update; 0.3 s sits exactly on a 100 ms step), and the last
/// one comes with the completion.
#[test]
fn frame_events_fire_once_in_the_update_that_crosses_them() {
    let mut app = app();
    let def = anim_with_events(&[(0.0, "start"), (0.25, "quarter"), (0.3, "edge"), (1.0, "end")]);
    let e = app
        .world_mut()
        .spawn((Opacity(0.0), def.to_tween(false, None).unwrap()))
        .id();
    let total = def.playback_duration(None);
    let mut seen: Vec<String> = Vec::new();
    let mut finished = 0;
    for _ in 0..40 {
        let before = app.world().get::<UiTween>(e).map_or(total, |t| t.elapsed);
        app.update();
        let after = app.world().get::<UiTween>(e).map_or(total, |t| t.elapsed);
        let fired = fired_this_update(&app);
        for (entity, name, payload) in &fired {
            assert_eq!(*entity, e);
            assert_eq!(*payload, format!("p-{name}"));
            let at = def
                .events
                .iter()
                .find(|ev| ev.name == *name)
                .map(|ev| ev.time)
                .unwrap();
            assert!(
                at <= after && (at > before || (at <= 0.0 && before <= 0.0)),
                "{name} at {at} fired while elapsed moved {before} -> {after}"
            );
            seen.push(name.clone());
        }
        if finished_this_update(&app).contains(&e) {
            finished += 1;
            assert_eq!(
                fired.last().map(|f| f.1.as_str()),
                Some("end"),
                "the last event comes with the completion"
            );
        }
        if app.world().get::<UiTween>(e).is_none() {
            break;
        }
    }
    assert_eq!(seen, ["start", "quarter", "edge", "end"], "each once, in order");
    assert_eq!(finished, 1);
    for _ in 0..3 {
        app.update();
        assert!(fired_this_update(&app).is_empty(), "nothing after the end");
    }
    let log = app.world().resource::<regia_ui::UiAnimEventLog>();
    assert_eq!(log.total, 4);
    assert_eq!(log.recent.len(), 4);
}

/// One long frame that jumps over several events and the end still fires each of them exactly once
/// (a skipped frame), and a looping animation fires its events once per pass.
#[test]
fn a_skipped_frame_fires_every_crossed_event_once_and_loops_repeat_them() {
    let mut app = app();
    let mut def = anim_with_events(&[(0.1, "a"), (0.2, "b"), (0.9, "c")]);
    def.mode = regia_ui::anim_timeline::LoopMode::Loop;
    def.loops = 3;
    let e = app
        .world_mut()
        .spawn((Opacity(0.0), def.to_tween(false, None).unwrap()))
        .id();
    // The first update starts the clock; then one 2.5 s frame jumps two and a half passes.
    app.update();
    let mut names: Vec<String> = fired_this_update(&app).into_iter().map(|f| f.1).collect();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(2500)));
    app.update();
    names.extend(fired_this_update(&app).into_iter().map(|f| f.1));
    assert_eq!(names, ["a", "b", "c", "a", "b", "c", "a", "b"], "{names:?}");
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(STEP_MS)));
    for _ in 0..40 {
        app.update();
        names.extend(fired_this_update(&app).into_iter().map(|f| f.1));
        if app.world().get::<UiTween>(e).is_none() {
            break;
        }
    }
    assert_eq!(names.len(), 9, "3 events x 3 passes, each once: {names:?}");
    assert_eq!(names.last().map(String::as_str), Some("c"));
}

/// Under reduced motion a plain animation jumps to its end in one update: every event still fires,
/// once.
#[test]
fn reduced_motion_jumps_to_the_end_and_still_fires_every_event_once() {
    let mut app = app();
    app.update();
    app.world_mut()
        .get_resource_or_insert_with(regia_ui::UiAccessibility::default)
        .reduced_motion = true;
    let def = anim_with_events(&[(0.5, "mid"), (1.0, "end")]);
    let e = app
        .world_mut()
        .spawn((Opacity(0.0), def.to_tween(false, None).unwrap()))
        .id();
    app.update();
    let names: Vec<String> = fired_this_update(&app).into_iter().map(|f| f.1).collect();
    assert_eq!(names, ["mid", "end"]);
    assert!(app.world().get::<UiTween>(e).is_none());
    app.update();
    assert!(fired_this_update(&app).is_empty());
}
