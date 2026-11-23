// SPDX-License-Identifier: MPL-2.0
//! The player's behavior with a counting scene: lazy drawing, refusals that change nothing, events.

#[path = "common/standin.rs"]
mod standin;

use std::cell::Cell;
use std::rc::Rc;

use regia_web::player::{Player, PlayerEvent};
use regia_web::seams::{
    CameraPose, DrawOutcome, PointerClass, ProductModel, SceneBackend, ViewModel, ViewRequest, ViewValues,
};
use regia_web::{Schedule, WebError};
use serde_json::json;
use standin::{StandInProduct, StandInView};

#[derive(Clone, Default)]
struct Counts {
    draws: Rc<Cell<u32>>,
    syncs: Rc<Cell<u32>>,
    geometry_syncs: Rc<Cell<u32>>,
    resizes: Rc<Cell<u32>>,
    last_size: Rc<Cell<[u32; 2]>>,
    /// Pipelines the scene creates beyond its warm set of 3.
    extra_pipelines: Rc<Cell<usize>>,
}

struct CountingScene {
    counts: Counts,
    outcome: Rc<Cell<Option<DrawOutcome>>>,
}

impl SceneBackend for CountingScene {
    fn sync_product(&mut self, _product: &dyn ProductModel, geometry: bool) -> Result<(), WebError> {
        self.counts.syncs.set(self.counts.syncs.get() + 1);
        if geometry {
            self.counts
                .geometry_syncs
                .set(self.counts.geometry_syncs.get() + 1);
        }
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.counts.resizes.set(self.counts.resizes.get() + 1);
        self.counts.last_size.set([width, height]);
    }

    fn draw(&mut self, _pose: &CameraPose) -> Result<DrawOutcome, WebError> {
        match self.outcome.get() {
            Some(o) => {
                if o == DrawOutcome::Drawn {
                    self.counts.draws.set(self.counts.draws.get() + 1);
                }
                Ok(o)
            }
            None => Err(WebError::new("render_failed", "refused")),
        }
    }

    fn pipelines(&self) -> usize {
        3 + self.counts.extra_pipelines.get()
    }
}

struct Rig {
    player: Player,
    counts: Counts,
    outcome: Rc<Cell<Option<DrawOutcome>>>,
    now: f64,
}

impl Rig {
    fn new() -> Self {
        let counts = Counts::default();
        let outcome = Rc::new(Cell::new(Some(DrawOutcome::Drawn)));
        let mut player = Player::new(Box::new(|| 0.0));
        player.attach_scene(Box::new(CountingScene {
            counts: counts.clone(),
            outcome: outcome.clone(),
        }));
        Rig {
            player,
            counts,
            outcome,
            now: 1000.0,
        }
    }

    fn opened() -> Self {
        let mut rig = Self::new();
        rig.settle();
        rig.player.resize([400.0, 400.0], [400, 400]);
        rig.player
            .install(Box::new(StandInProduct::new()), Box::new(StandInView::new()))
            .expect("installs");
        rig.player.draw_first();
        rig.settle();
        rig.player.drain_events();
        rig
    }

    fn draws(&self) -> u32 {
        self.counts.draws.get()
    }

    /// Runs browser frames for as long as one is pending.
    fn settle(&mut self) -> u32 {
        let mut frames = 0;
        while self.player.lazy().pending() {
            self.now += 16.0;
            self.player.frame(self.now);
            frames += 1;
            assert!(frames < 10_000, "the loop never stopped");
        }
        frames
    }
}

fn changed_causes(events: &[PlayerEvent]) -> Vec<&'static str> {
    events
        .iter()
        .filter_map(|e| match e {
            PlayerEvent::Changed { cause, .. } => Some(*cause),
            _ => None,
        })
        .collect()
}

#[test]
fn nothing_draws_before_a_product_opens() {
    let mut rig = Rig::new();
    rig.settle();
    assert_eq!(rig.draws(), 0);
    assert!(rig.player.drain_events().is_empty());
}

#[test]
fn open_draws_at_once_and_says_ready_once() {
    let mut rig = Rig::new();
    rig.player
        .install(Box::new(StandInProduct::new()), Box::new(StandInView::new()))
        .expect("installs");
    assert!(rig.player.drain_events().is_empty(), "ready waits for the frame");
    rig.player.draw_first();
    assert_eq!(rig.draws(), 1);
    let events = rig.player.drain_events();
    assert_eq!(events, vec![PlayerEvent::Ready { incomplete: false }]);
    rig.settle();
    assert!(rig.player.drain_events().is_empty());
}

#[test]
fn ready_waits_until_a_frame_really_reached_the_canvas() {
    let mut rig = Rig::new();
    rig.outcome.set(Some(DrawOutcome::Skipped));
    rig.player
        .install(Box::new(StandInProduct::new()), Box::new(StandInView::new()))
        .expect("installs");
    rig.player.draw_first();
    assert!(rig.player.drain_events().is_empty());
    rig.outcome.set(Some(DrawOutcome::Drawn));
    rig.player.resize([10.0, 10.0], [10, 10]);
    rig.settle();
    assert_eq!(
        rig.player.drain_events(),
        vec![PlayerEvent::Ready { incomplete: false }]
    );
}

#[test]
fn idle_is_zero_draws_and_nothing_pending() {
    let mut rig = Rig::opened();
    let before = rig.draws();
    assert!(!rig.player.lazy().pending());
    assert_eq!(rig.settle(), 0);
    assert_eq!(rig.draws(), before);
}

#[test]
fn a_pick_draws_one_frame_and_reports_the_build() {
    let mut rig = Rig::opened();
    let before = rig.draws();
    let (result, schedule) = rig
        .player
        .set_pick("Material", &json!("Rose Gold"), false)
        .expect("sold");
    assert_eq!(schedule, Schedule::Request);
    assert_eq!(result.build["picks"]["Material"], "Rose Gold");
    assert!(result.adjusted.is_empty());
    rig.settle();
    assert_eq!(rig.draws(), before + 1);
    assert_eq!(changed_causes(&rig.player.drain_events()), ["pick"]);
}

#[test]
fn a_refused_pick_changes_nothing_at_all() {
    let mut rig = Rig::opened();
    rig.player.set_pick("Gem", &json!("Ruby"), false).expect("sold");
    rig.settle();
    rig.player.drain_events();
    let before_build = rig.player.build().expect("open");
    let before_draws = rig.draws();
    let before_syncs = rig.counts.syncs.get();

    let e = rig
        .player
        .set_pick("Material", &json!("Titanium"), false)
        .expect_err("not sold");
    assert_eq!(e.code, "pick_not_sold");

    assert_eq!(rig.player.build().expect("open"), before_build);
    assert!(!rig.player.lazy().pending(), "a refusal schedules no frame");
    rig.settle();
    assert_eq!(rig.draws(), before_draws);
    assert_eq!(rig.counts.syncs.get(), before_syncs);
    let events = rig.player.drain_events();
    assert!(
        matches!(
            events.as_slice(),
            [PlayerEvent::Error(WebError {
                code: "pick_not_sold",
                ..
            })]
        ),
        "{events:?}"
    );
    let stats = rig.player.stats("test");
    assert_eq!(stats["refused"], 1);
}

#[test]
fn unknown_groups_and_values_have_their_own_codes() {
    let mut rig = Rig::opened();
    let e = rig
        .player
        .set_pick("Nope", &json!("x"), false)
        .expect_err("unknown");
    assert_eq!(e.code, "pick_unknown_group");
    let e = rig
        .player
        .set_pick("Material", &json!("Tin"), false)
        .expect_err("unknown");
    assert_eq!(e.code, "pick_unknown_value");
    let e = rig
        .player
        .set_pick("Material", &json!(5), false)
        .expect_err("a number");
    assert_eq!(e.code, "pick_unknown_value");
}

#[test]
fn adjust_names_the_groups_it_changed() {
    let mut rig = Rig::opened();
    rig.player.set_pick("Gem", &json!("Ruby"), false).expect("sold");
    let (r, _) = rig
        .player
        .set_pick("Material", &json!("Titanium"), true)
        .expect("adjusted");
    assert_eq!(r.adjusted, ["Gem"]);
    assert_eq!(r.build["picks"]["Material"], "Titanium");
    assert_eq!(r.build["picks"]["Gem"], "Clear");
}

#[test]
fn material_only_keeps_meshes_and_geometry_says_so() {
    let mut rig = Rig::opened();
    let before = rig.counts.geometry_syncs.get();
    rig.player
        .set_pick("Material", &json!("Rose Gold"), false)
        .expect("sold");
    assert_eq!(rig.counts.geometry_syncs.get(), before);
    rig.player.set_pick("Gauge", &json!("14"), false).expect("sold");
    assert_eq!(rig.counts.geometry_syncs.get(), before + 1);
}

#[test]
fn a_drag_animates_then_stops_and_says_changed_once() {
    let mut rig = Rig::opened();
    let before = rig.draws();
    rig.player.pointer_down(1, PointerClass::Touch, 200.0, 200.0, 0.0);
    rig.player.pointer_move(1, 260.0, 200.0, 16.0);
    rig.player.pointer_up(1, 32.0);
    let frames = rig.settle();
    assert!(frames > 2, "damping takes several frames, got {frames}");
    assert!(rig.draws() > before);
    assert!(!rig.player.animating());
    assert_eq!(changed_causes(&rig.player.drain_events()), ["view"]);
    assert_eq!(rig.settle(), 0, "the loop is idle again");
    let view = rig.player.view().expect("open");
    assert!(view["tilt"][0].as_f64().expect("number") > 1.0, "{view}");
}

#[test]
fn input_that_moves_nothing_draws_nothing() {
    let mut rig = Rig::opened();
    let before = rig.draws();
    rig.player.pointer_down(1, PointerClass::Mouse, 10.0, 10.0, 0.0);
    rig.player.pointer_up(1, 5.0);
    rig.settle();
    assert_eq!(rig.draws(), before);
    assert!(rig.player.drain_events().is_empty());
}

#[test]
fn the_tilt_limit_holds_and_set_view_reports_the_clamp() {
    let mut rig = Rig::opened();
    let request: ViewRequest =
        serde_json::from_value(json!({"tilt": [80.0, 0.0], "animate": false})).expect("shape");
    let (report, _) = rig.player.set_view(&request).expect("open");
    assert_eq!(report["clamped"], true);
    assert!((report["view"]["tilt"][0].as_f64().expect("number") - 30.0).abs() < 1e-3);
    rig.settle();
    let request: ViewRequest = serde_json::from_value(json!({"tilt": [10.0, 5.0]})).expect("shape");
    let (report, _) = rig.player.set_view(&request).expect("open");
    assert_eq!(report["clamped"], false);
}

#[test]
fn set_view_refuses_unknown_fields_at_the_boundary() {
    assert!(serde_json::from_value::<ViewRequest>(json!({"tilit": [1.0, 2.0]})).is_err());
}

#[test]
fn view_calls_need_a_product() {
    let mut rig = Rig::new();
    assert_eq!(rig.player.view().expect_err("nothing open").code, "no_product");
    assert_eq!(rig.player.options().expect_err("nothing open").code, "no_product");
    assert_eq!(rig.player.build().expect_err("nothing open").code, "no_product");
    assert_eq!(
        rig.player.reset_view().expect_err("nothing open").code,
        "no_product"
    );
    let e = rig
        .player
        .set_pick("Gem", &json!("Ruby"), false)
        .expect_err("nothing open");
    assert_eq!(e.code, "no_product");
    assert_eq!(
        rig.player
            .set_view(&ViewRequest::default())
            .expect_err("nothing open")
            .code,
        "no_product"
    );
}

#[test]
fn resize_redraws_once_and_the_same_size_does_nothing() {
    let mut rig = Rig::opened();
    let before = rig.draws();
    assert_eq!(rig.player.resize([300.0, 200.0], [600, 400]), Schedule::Request);
    rig.settle();
    assert_eq!(rig.draws(), before + 1);
    assert_eq!(rig.counts.last_size.get(), [600, 400]);
    let resizes = rig.counts.resizes.get();
    assert_eq!(rig.player.resize([300.0, 200.0], [600, 400]), Schedule::Nothing);
    rig.settle();
    assert_eq!(rig.counts.resizes.get(), resizes);
    assert_eq!(rig.draws(), before + 1);
}

#[test]
fn a_resize_can_draw_at_once() {
    let mut rig = Rig::opened();
    let before = rig.draws();
    rig.player.resize([300.0, 200.0], [600, 400]);
    assert!(rig.player.draw_now(), "the frame goes out before the next paint");
    assert_eq!(rig.draws(), before + 1);
    assert_eq!(rig.counts.last_size.get(), [600, 400]);
    rig.settle();
    assert_eq!(
        rig.draws(),
        before + 1,
        "the frame that was requested finds nothing to do"
    );
}

#[test]
fn draw_now_respects_hidden_and_deterministic() {
    let mut rig = Rig::opened();
    let before = rig.draws();
    rig.player.set_visible(false);
    rig.player.resize([300.0, 200.0], [600, 400]);
    assert!(!rig.player.draw_now());
    rig.player.set_visible(true);
    rig.settle();
    let shown = rig.draws();
    assert!(shown > before);
    rig.player.set_deterministic(true);
    rig.player.resize([100.0, 100.0], [100, 100]);
    assert!(!rig.player.draw_now(), "only step() draws in deterministic mode");
    assert_eq!(rig.draws(), shown);
    assert!(
        rig.player.step(16.0).expect("deterministic"),
        "the resize is drawn by the next step"
    );
}

#[test]
fn hidden_draws_nothing_and_showing_draws_once() {
    let mut rig = Rig::opened();
    let before = rig.draws();
    rig.player.set_visible(false);
    rig.player
        .set_pick("Material", &json!("Rose Gold"), false)
        .expect("sold while hidden");
    rig.player.pointer_down(1, PointerClass::Touch, 1.0, 1.0, 0.0);
    rig.player.pointer_move(1, 100.0, 1.0, 1.0);
    rig.player.pointer_up(1, 2.0);
    assert!(!rig.player.lazy().pending());
    assert_eq!(rig.draws(), before);
    rig.player.set_visible(true);
    rig.settle();
    assert!(rig.draws() > before);
    assert!(!rig.player.lazy().pending());
}

/// Correctness audit (corr-web-viewer): a device restore that finishes after `destroy` used to attach its scene,
/// keeping the GPU objects alive and asking for frames on a destroyed player.
#[test]
fn corr_a_scene_attached_after_destroy_is_dropped() {
    let mut rig = Rig::opened();
    rig.player.device_lost("test");
    rig.player.destroy();
    let schedule = rig.player.attach_scene(Box::new(CountingScene {
        counts: Counts::default(),
        outcome: rig.outcome.clone(),
    }));
    assert_eq!(schedule, Schedule::Nothing);
    assert!(!rig.player.has_scene(), "a destroyed player holds no scene");
}

/// Correctness audit (corr-web-viewer): a loss that lands while a browser frame is pending (any animation) has
/// the host cancel that frame. The player used to keep it pending, so the restored scene, every input and every
/// pick answered `Nothing` and the canvas never drew again.
#[test]
fn corr_a_loss_with_a_frame_pending_lets_the_restored_scene_ask_for_one() {
    let mut rig = Rig::opened();
    assert_eq!(rig.player.invalidate(), Schedule::Request);
    assert!(rig.player.lazy().pending());
    // The host cancels its frame and tells the player.
    rig.player.device_lost("test");
    assert!(!rig.player.lazy().pending(), "the cancelled frame is not pending");
    let counts = Counts::default();
    let schedule = rig.player.attach_scene(Box::new(CountingScene {
        counts: counts.clone(),
        outcome: rig.outcome.clone(),
    }));
    assert_eq!(schedule, Schedule::Request, "the restored scene asks for a frame");
    rig.settle();
    assert!(counts.draws.get() >= 1, "the restored scene draws");
    // And input after the restore still asks for frames.
    assert_eq!(
        rig.player.pointer_down(1, PointerClass::Touch, 1.0, 1.0, 0.0),
        Schedule::Request
    );
}

#[test]
fn a_lost_device_stops_drawing_and_a_new_scene_restores_the_product() {
    let mut rig = Rig::opened();
    rig.player.device_lost("test");
    assert!(!rig.player.has_scene());
    let events = rig.player.drain_events();
    assert!(matches!(
        events.as_slice(),
        [PlayerEvent::Error(WebError {
            code: "device_lost",
            ..
        })]
    ));
    // Picks still work on the kept product; nothing draws.
    rig.player
        .set_pick("Gem", &json!("Sapphire"), false)
        .expect("sold");
    rig.settle();
    let counts = Counts::default();
    rig.player.attach_scene(Box::new(CountingScene {
        counts: counts.clone(),
        outcome: rig.outcome.clone(),
    }));
    rig.settle();
    assert_eq!(counts.draws.get(), 1);
    assert_eq!(counts.geometry_syncs.get(), 1, "everything is shown again");
    assert_eq!(counts.last_size.get(), [400, 400]);
}

#[test]
fn a_render_failure_is_an_event_and_does_not_spin() {
    let mut rig = Rig::opened();
    rig.outcome.set(None);
    rig.player
        .set_pick("Gem", &json!("Sapphire"), false)
        .expect("sold");
    let frames = rig.settle();
    assert_eq!(frames, 1);
    let events = rig.player.drain_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            PlayerEvent::Error(WebError {
                code: "render_failed",
                ..
            })
        )),
        "{events:?}"
    );
}

#[test]
fn deterministic_steps_repeat_exactly_and_the_browser_is_never_asked() {
    fn script() -> (Vec<regia_web::seams::ViewValues>, u32) {
        let mut rig = Rig::opened();
        rig.player.set_deterministic(true);
        rig.player.pointer_down(1, PointerClass::Touch, 200.0, 200.0, 0.0);
        rig.player.pointer_move(1, 233.0, 170.0, 16.0);
        rig.player.pointer_up(1, 32.0);
        assert!(!rig.player.lazy().pending());
        let mut shown = Vec::new();
        for _ in 0..12 {
            rig.player.step(16.0).expect("deterministic");
            let v = rig.player.view().expect("open");
            shown.push(serde_json::from_value(v).expect("shape"));
        }
        (shown, rig.draws())
    }
    let a = script();
    let b = script();
    assert_eq!(a, b);
    assert!(a.0.windows(2).any(|w| w[0] != w[1]), "the view moved");
}

#[test]
fn step_refuses_outside_deterministic_mode_and_bad_times() {
    let mut rig = Rig::opened();
    assert_eq!(rig.player.step(16.0).expect_err("not on").code, "bad_argument");
    rig.player.set_deterministic(true);
    assert_eq!(rig.player.step(-1.0).expect_err("negative").code, "bad_argument");
    assert_eq!(rig.player.step(f64::NAN).expect_err("nan").code, "bad_argument");
    assert!(!rig.player.step(0.0).expect("zero is fine"));
}

#[test]
fn leaving_deterministic_mode_resumes_the_lazy_loop() {
    let mut rig = Rig::opened();
    rig.player.set_deterministic(true);
    rig.player
        .set_pick("Gem", &json!("Sapphire"), false)
        .expect("sold");
    assert!(!rig.player.lazy().pending());
    assert_eq!(rig.player.set_deterministic(false), Schedule::Request);
    rig.settle();
    assert!(!rig.player.lazy().dirty());
}

#[test]
fn destroy_stops_everything_and_later_calls_say_destroyed() {
    let mut rig = Rig::opened();
    rig.player.pointer_down(1, PointerClass::Touch, 1.0, 1.0, 0.0);
    rig.player.pointer_move(1, 100.0, 1.0, 1.0);
    rig.player.destroy();
    assert!(!rig.player.has_scene());
    assert!(!rig.player.lazy().pending());
    assert_eq!(rig.settle(), 0);
    assert_eq!(rig.player.options().expect_err("gone").code, "destroyed");
    assert_eq!(
        rig.player
            .set_pick("Gem", &json!("Ruby"), false)
            .expect_err("gone")
            .code,
        "destroyed"
    );
    assert_eq!(
        rig.player.pointer_down(1, PointerClass::Touch, 1.0, 1.0, 0.0),
        Schedule::Nothing
    );
}

#[test]
fn a_second_open_replaces_the_first() {
    let mut rig = Rig::opened();
    rig.player.set_pick("Gem", &json!("Ruby"), false).expect("sold");
    rig.settle();
    rig.player.drain_events();
    rig.player
        .install(Box::new(StandInProduct::new()), Box::new(StandInView::new()))
        .expect("installs");
    rig.player.draw_first();
    assert_eq!(rig.player.build().expect("open")["picks"]["Gem"], "Clear");
    assert_eq!(
        rig.player.drain_events(),
        vec![PlayerEvent::Ready { incomplete: false }]
    );
}

#[test]
fn stats_carry_the_idle_truth() {
    let mut rig = Rig::opened();
    let s = rig.player.stats("webgl2");
    assert_eq!(s["backend"], "webgl2");
    assert_eq!(s["pending"], false);
    assert_eq!(s["pipelines"], 3);
    assert_eq!(s["drawing"], json!([400, 400]));
    assert!(s["draws"].as_u64().expect("number") >= 1);
    let _ = rig.settle();
}

/// A view that settles on the next `update`, whatever the step was. Used to pin the frame where a
/// gesture's last picture is the one the surface skipped.
struct SettleOnce {
    shown: ViewValues,
    target: ViewValues,
    armed: bool,
}

impl SettleOnce {
    fn new() -> Self {
        Self {
            shown: ViewValues::default(),
            target: ViewValues::default(),
            armed: false,
        }
    }
}

impl ViewModel for SettleOnce {
    fn pointer_down(&mut self, _id: u64, _class: PointerClass, _x: f32, _y: f32, _t_ms: f64) {}

    fn pointer_move(&mut self, _id: u64, _x: f32, _y: f32, _t_ms: f64) {}

    fn pointer_up(&mut self, _id: u64, _t_ms: f64) {}

    fn pointer_cancel(&mut self, _id: u64) {}

    fn wheel(&mut self, _lines: f32) {}

    fn set_viewport(&mut self, _width: f32, _height: f32) {}

    fn update(&mut self, _dt_s: f32) -> bool {
        if !self.armed {
            return false;
        }
        self.shown = self.target;
        self.armed = false;
        true
    }

    fn is_animating(&self) -> bool {
        self.armed
    }

    fn set_view(&mut self, view: ViewValues, immediate: bool) -> ViewValues {
        self.target = view;
        if immediate {
            self.shown = view;
            self.armed = false;
        } else {
            self.armed = self.shown != view;
        }
        self.target
    }

    fn reset(&mut self) {
        self.target = ViewValues::default();
        self.armed = self.shown != self.target;
    }

    fn shown(&self) -> ViewValues {
        self.shown
    }

    fn pose(&self) -> CameraPose {
        CameraPose {
            eye: [0.0, 0.0, 60.0],
            look_at: [0.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            fov_short_deg: 20.0,
            near: 1.0,
            far: 200.0,
        }
    }
}

// Audit fix (sprint-fix-audit-jw, finding 2): a frame the surface skipped (timeout, outdated,
// occluded) is retried on the next browser frame, so `ready` and a new pick never wait for an input.

/// A gesture (or an animated `setView`) can settle on the same frame the surface skips. That frame is
/// not `scene_dirty` — the draw happened because the view moved — so the retry, which looks at
/// `scene_dirty`, used to go idle and leave the old picture up until the next input.
#[test]
fn audit_settling_skip_is_retried() {
    let mut rig = Rig::new();
    rig.settle();
    rig.player
        .install(Box::new(StandInProduct::new()), Box::new(SettleOnce::new()))
        .expect("installs");
    rig.player.draw_first();
    rig.settle();
    rig.player.drain_events();
    let before = rig.draws();
    let request: ViewRequest =
        serde_json::from_value(json!({"tilt": [4.0, 0.0], "animate": true})).expect("shape");
    rig.player.set_view(&request).expect("open");
    rig.outcome.set(Some(DrawOutcome::Skipped));
    rig.now += 16.0;
    let schedule = rig.player.frame(rig.now);
    assert_eq!(rig.draws(), before, "the surface skipped the settling frame");
    assert!(!rig.player.animating(), "the gesture has settled");
    assert_eq!(
        schedule,
        Schedule::Request,
        "a skipped settling frame is drawn again without more input"
    );
    rig.outcome.set(Some(DrawOutcome::Drawn));
    rig.settle();
    assert_eq!(rig.draws(), before + 1, "the retry reached the surface");
    assert_eq!(rig.settle(), 0);
}

#[test]
fn a_skipped_first_frame_is_retried_without_any_input() {
    let mut rig = Rig::new();
    rig.settle();
    rig.outcome.set(Some(DrawOutcome::Skipped));
    rig.player
        .install(Box::new(StandInProduct::new()), Box::new(StandInView::new()))
        .expect("installs");
    let _ = rig.player.draw_first();
    assert!(rig.player.drain_events().is_empty());
    assert!(
        rig.player.lazy().pending(),
        "a skipped first frame asks for another browser frame"
    );
    rig.outcome.set(Some(DrawOutcome::Drawn));
    rig.settle();
    assert_eq!(
        rig.player.drain_events(),
        vec![PlayerEvent::Ready { incomplete: false }]
    );
    assert_eq!(rig.draws(), 1);
    assert_eq!(rig.settle(), 0, "nothing is pending once drawn");
}

#[test]
fn a_pick_that_lands_on_a_skipped_frame_is_drawn_on_the_next_one() {
    let mut rig = Rig::opened();
    let before = rig.draws();
    rig.outcome.set(Some(DrawOutcome::Skipped));
    rig.player
        .set_pick("Material", &json!("Rose Gold"), false)
        .expect("sold");
    rig.now += 16.0;
    rig.player.frame(rig.now);
    assert_eq!(rig.draws(), before, "the surface skipped the frame");
    assert!(rig.player.lazy().pending(), "the skipped frame is retried");
    rig.outcome.set(Some(DrawOutcome::Drawn));
    rig.settle();
    assert_eq!(rig.draws(), before + 1);
    assert_eq!(rig.settle(), 0);
}

#[test]
fn a_surface_that_never_comes_back_stops_the_retries_until_the_next_change() {
    let mut rig = Rig::opened();
    rig.outcome.set(Some(DrawOutcome::Skipped));
    rig.player
        .set_pick("Material", &json!("Rose Gold"), false)
        .expect("sold");
    let frames = rig.settle();
    assert_eq!(frames, regia_web::player::MAX_SKIPPED_RETRIES + 1);
    assert!(!rig.player.lazy().pending(), "the page is idle again");
    // A new change starts the retries over, and the frame is drawn once the surface is back.
    let before = rig.draws();
    rig.player.invalidate();
    rig.now += 16.0;
    rig.player.frame(rig.now);
    assert!(rig.player.lazy().pending());
    rig.outcome.set(Some(DrawOutcome::Drawn));
    rig.settle();
    assert_eq!(rig.draws(), before + 1);
}

/// Spec 42 § 7: every pipeline is built while a product opens. One created after the first frame is a
/// `warm_set_miss` entry in `stats().problems`; a new open takes its own warm set at its first frame.
#[test]
fn a_pipeline_created_after_the_first_frame_is_a_warm_set_miss() {
    let mut rig = Rig::opened();
    assert_eq!(rig.player.stats("webgl2")["problems"], json!([]));
    rig.counts.extra_pipelines.set(1);
    let problems = rig.player.stats("webgl2")["problems"].clone();
    assert_eq!(problems.as_array().map(Vec::len), Some(1), "{problems}");
    assert_eq!(
        problems[0],
        "warm_set_miss: 1 pipelines created after the first frame (3 warm, 4 now)"
    );
    rig.player
        .install(Box::new(StandInProduct::new()), Box::new(StandInView::new()))
        .expect("installs");
    assert_eq!(
        rig.player.stats("webgl2")["problems"],
        json!([]),
        "no warm set before the new product's first frame"
    );
    rig.player.draw_first();
    rig.settle();
    assert_eq!(rig.player.stats("webgl2")["problems"], json!([]));
    rig.counts.extra_pipelines.set(2);
    assert_eq!(
        rig.player.stats("webgl2")["problems"][0],
        "warm_set_miss: 1 pipelines created after the first frame (4 warm, 5 now)"
    );
}
