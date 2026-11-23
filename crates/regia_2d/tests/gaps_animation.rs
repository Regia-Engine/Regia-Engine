// SPDX-License-Identifier: MPL-2.0
//! Sprite-sheet animation edge cases (test-gap audit): exact frame boundaries, looping versus
//! play-once, negative and NaN time steps, frame jumps, and the `animate_sprites` system driven
//! headlessly with a manual clock. CPU only.

use std::time::Duration;

use regia_2d::{AnimatedSprite, Animation, Sprite, Sprite2dPlugin, SpriteSource};
use regia_app::prelude::*;

/// 8 fps has an exactly representable period of 0.125 s, so boundaries are exact in binary.
fn idle4() -> AnimatedSprite {
    AnimatedSprite::new(15, 20, &[Animation::new("idle", 0, 4, 8.0)], true)
}

/// Catches an off-by-one at the step boundary: exactly one period is one frame, just under is none.
/// The steps are exact in binary (7/64 s + 1/64 s): the clock counts whole nanoseconds of each
/// `dt` (determinism audit 11-08), and `0.124_f32 + 0.001_f32` is 124 999 998 ns, which is under
/// the period.
#[test]
fn a_step_happens_exactly_at_the_period_and_not_a_hair_before() {
    let mut s = idle4();
    s.update(0.109_375);
    assert_eq!(s.frame_index(), 0);
    s.update(0.015_625);
    assert_eq!(s.frame_index(), 1, "0.125 s of the clock is one whole period");

    let mut under = idle4();
    under.update(0.124_999);
    assert_eq!(
        under.frame_index(),
        0,
        "a microsecond under the period is no step"
    );

    let mut exact = idle4();
    exact.update(0.125);
    assert_eq!(exact.frame_index(), 1);
}

/// Catches a loop that stops one frame early or late: N frames take N steps to come back to 0.
#[test]
fn a_looping_animation_wraps_after_exactly_its_frame_count() {
    let mut s = idle4();
    for expected in [1, 2, 3, 0, 1] {
        s.update(0.125);
        assert_eq!(s.frame_index(), expected);
    }
    assert!(!s.finished(), "a loop never finishes");
    let mut whole = idle4();
    whole.update(0.5);
    assert_eq!(
        whole.frame_index(),
        0,
        "four steps in one update wrap to the start"
    );
}

/// Catches a lost remainder: two half periods must make one step even across separate updates.
#[test]
fn the_remainder_of_a_step_carries_over_to_the_next_update() {
    let mut s = idle4();
    s.update(0.0625);
    assert_eq!(s.frame_index(), 0);
    s.update(0.0625);
    assert_eq!(s.frame_index(), 1);
    s.update(0.1875);
    assert_eq!(
        s.frame_index(),
        2,
        "0.1875 s is one and a half periods: one step, half kept"
    );
    s.update(0.0625);
    assert_eq!(s.frame_index(), 3);
}

/// Catches negative or NaN time steps running the clock backwards or poisoning it for good.
#[test]
fn negative_and_nan_time_steps_change_nothing() {
    let mut s = idle4();
    s.update(0.1);
    s.update(-5.0);
    s.update(f32::NAN);
    assert_eq!(s.frame_index(), 0);
    // The 0.1 s already spent must still count: 0.1 + 0.03 crosses 0.125.
    s.update(0.03);
    assert_eq!(s.frame_index(), 1);
}

/// Catches `finished` being set one step too early (the last frame must be shown for a full
/// period first) or never.
#[test]
fn a_play_once_animation_shows_its_last_frame_before_it_finishes() {
    let mut s = AnimatedSprite::new(8, 8, &[Animation::new("hit", 1, 3, 8.0).once()], true);
    s.update(0.25);
    assert_eq!(s.frame_index(), 2);
    assert!(s.is_last_frame());
    assert!(
        !s.finished(),
        "the last frame has not been shown for a full period yet"
    );
    s.update(0.125);
    assert_eq!(s.frame_index(), 2, "holds the last frame");
    assert!(s.finished());
    s.update(10.0);
    assert_eq!((s.frame_index(), s.finished()), (2, true));
    // A finished one-shot restarts when asked for again.
    assert!(s.play("hit"));
    assert_eq!((s.frame_index(), s.finished()), (0, false));
}

/// Catches `set_frame` not wrapping into range, keeping a stale clock, or leaving `finished` set.
#[test]
fn set_frame_wraps_resets_the_clock_and_clears_finished() {
    let mut s = AnimatedSprite::new(8, 8, &[Animation::new("hit", 0, 3, 8.0).once()], true);
    s.update(100.0);
    assert!(s.finished());
    s.set_frame(7);
    assert_eq!(s.frame_index(), 1, "7 % 3");
    assert!(!s.finished());
    // The clock restarted: 0.1 s alone is not a step.
    s.update(0.1);
    assert_eq!(s.frame_index(), 1);
    s.update(0.025);
    assert_eq!(s.frame_index(), 2);
}

/// Catches an invalid animation index changing the state, and `play` of an unknown name
/// altering `playing` or the current clip.
#[test]
fn unknown_animations_are_ignored_and_play_by_name_restarts_a_different_clip() {
    let mut s = AnimatedSprite::new(
        8,
        8,
        &[
            Animation::new("idle", 0, 4, 8.0),
            Animation::new("run", 1, 6, 8.0),
        ],
        false,
    );
    s.set_animation(9);
    assert_eq!(s.current_animation(), 0);
    assert!(!s.play("nope"));
    assert!(!s.playing, "a failed play must not start the clock");
    assert_eq!(s.current_animation(), 0);

    s.set_frame(3);
    assert!(s.play("run"));
    assert!(s.playing);
    assert_eq!((s.current_animation(), s.frame_index()), (1, 0));
    assert_eq!(s.frame().source_rect, [0, 8, 8, 8]);
}

/// Catches a zero-frame animation (division by zero) and single-frame clips moving.
#[test]
fn degenerate_frame_counts_stay_on_frame_zero() {
    assert_eq!(Animation::new("z", 0, 0, 8.0).frames, 1);
    let mut one = AnimatedSprite::new(4, 4, &[Animation::new("one", 0, 1, 30.0)], true);
    one.update(5.0);
    assert_eq!(one.frame_index(), 0);
    assert!(one.is_last_frame());
    // Public fields can still hold a zero (struct literal): updates must not panic or wrap.
    let mut raw = AnimatedSprite::new(
        4,
        4,
        &[Animation {
            frames: 0,
            ..Animation::default()
        }],
        true,
    );
    raw.update(5.0);
    assert_eq!(raw.frame_index(), 0);
}

/// Catches an empty animation list panicking or reporting a last frame.
#[test]
fn an_empty_animation_list_is_inert() {
    let mut s = AnimatedSprite::new(10, 12, &[], true);
    s.update(1.0);
    s.set_frame(5);
    assert!(s.animation().is_none());
    assert!(!s.is_last_frame());
    assert!(!s.play("x"));
    assert_eq!(s.frame().source_rect, [10 * s.frame_index(), 0, 10, 12]);
}

/// Catches the sheet offset being added to the frame index instead of the pixel origin, or the
/// row/column swapped.
#[test]
fn the_source_rect_is_offset_plus_tile_times_frame_and_row() {
    let mut s = AnimatedSprite::new(15, 20, &[Animation::new("a", 2, 4, 8.0)], true);
    s.sheet_offset = [3, 5];
    s.set_frame(2);
    assert_eq!(s.frame().source_rect, [3 + 30, 5 + 40, 15, 20]);
}

fn headless_app(step_ms: u64) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, Sprite2dPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(step_ms)));
    app
}

fn source_of(app: &App, e: Entity) -> SpriteSource {
    app.world().get::<Sprite>(e).expect("sprite").source.clone()
}

/// Catches the system skipping paused sprites (their rect must still be written once), advancing
/// an `AnimatedSprite` that has no `Sprite`, or stepping at the wrong rate against the real clock.
#[test]
fn animate_sprites_writes_the_frame_rect_and_follows_the_clock() {
    let mut app = headless_app(125);
    let moving = app
        .world_mut()
        .spawn((Sprite::rect("a", [0, 0, 1, 1]), idle4()))
        .id();
    let mut paused_anim = idle4();
    paused_anim.playing = false;
    paused_anim.set_frame(2);
    let paused = app.world_mut().spawn((Sprite::default(), paused_anim)).id();
    let bare = app.world_mut().spawn(idle4()).id();

    let mut seen = Vec::new();
    for _ in 0..8 {
        app.update();
        seen.push(
            app.world()
                .get::<AnimatedSprite>(moving)
                .expect("anim")
                .frame_index(),
        );
    }
    // One frame per 125 ms update, wrapping at 4; the first update may be a zero-length frame.
    let steps: Vec<u32> = seen.windows(2).map(|w| (w[1] + 4 - w[0]) % 4).collect();
    assert!(steps.iter().all(|&d| d == 1), "frames {seen:?}");
    let frame = *seen.last().expect("frames");
    assert_eq!(
        source_of(&app, moving),
        SpriteSource::Rect {
            rect: [15 * frame, 0, 15, 20]
        }
    );
    assert_eq!(
        source_of(&app, paused),
        SpriteSource::Rect {
            rect: [30, 0, 15, 20]
        },
        "a paused sprite still shows its current frame"
    );
    assert_eq!(
        app.world()
            .get::<AnimatedSprite>(paused)
            .expect("anim")
            .frame_index(),
        2
    );
    assert_eq!(
        app.world()
            .get::<AnimatedSprite>(bare)
            .expect("anim")
            .frame_index(),
        0
    );
}
