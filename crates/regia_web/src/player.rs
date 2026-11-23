// SPDX-License-Identifier: MPL-2.0
//! The player: picks, view, loop and events together, with no browser and no GPU in it.
//!
//! The host (the browser glue, or a test) calls these methods and does what the returned
//! [`Schedule`] says; events wait in a queue the host drains when it holds no borrow, so a JS listener
//! may call straight back into the player.

use serde_json::{Value, json};

use crate::WebError;
use crate::lazy::{LazyLoop, LoopCounters, Schedule};
use crate::seams::{
    CameraPose, DrawOutcome, PointerClass, ProductModel, SceneBackend, ViewModel, ViewRequest, ViewValues,
};

/// What the page hears about.
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    /// The product is open and its first frame is on the canvas.
    Ready {
        /// The listing is incomplete; never present it as the full range.
        incomplete: bool,
    },
    /// The picks changed, or the view came to rest somewhere new.
    Changed {
        /// `pick` or `view`.
        cause: &'static str,
        /// The new build.
        build: Value,
    },
    /// Something failed.
    Error(WebError),
}

/// The result of a successful `setPick`.
#[derive(Clone, Debug, PartialEq)]
pub struct PickResult {
    /// The new build.
    pub build: Value,
    /// Groups changed on top of the pick.
    pub adjusted: Vec<String>,
}

/// Why the player draws.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawCounters {
    /// Frames that reached the surface.
    pub draws: u64,
    /// Frames the surface skipped.
    pub skipped: u64,
    /// Picks applied.
    pub picks: u64,
    /// Picks refused.
    pub refused: u64,
}

/// How many browser frames in a row the player asks for again after the surface skipped one (a
/// timeout, an outdated or occluded surface), about two seconds at 60 Hz. After that it waits for the
/// next change, so a surface that never comes back cannot keep the page busy.
pub const MAX_SKIPPED_RETRIES: u32 = 120;

/// The player's state.
pub struct Player {
    lazy: LazyLoop,
    clock: Box<dyn Fn() -> f64>,
    scene: Option<Box<dyn SceneBackend>>,
    product: Option<Box<dyn ProductModel>>,
    view: Option<Box<dyn ViewModel>>,
    drawing: [u32; 2],
    css: [f32; 2],
    resize_pending: bool,
    scene_dirty: bool,
    ready_pending: bool,
    view_unreported: bool,
    switch_started: Option<f64>,
    last_switch_ms: Option<f64>,
    /// Time the script has stepped, ms, in deterministic mode: the player's only clock there.
    script_ms: f64,
    /// Frames the script has stepped in deterministic mode.
    script_frames: u64,
    destroyed: bool,
    /// Frames skipped in a row since the last one that reached the surface or the last change.
    skip_streak: u32,
    /// The pending browser frame was asked for only to retry a skipped one.
    retry_requested: bool,
    counters: DrawCounters,
    events: Vec<PlayerEvent>,
    /// The scene's pipeline count when the shown product's first frame (or the first frame after a
    /// new scene) reached the canvas: the warm set. More pipelines later is a `warm_set_miss` problem
    /// in [`Player::stats`] (spec 42 § 7).
    warm_pipelines: Option<usize>,
}

impl Player {
    /// A player with nothing open. `clock` reads the time in ms; it only feeds the switch timing, and
    /// deterministic mode replaces it with the script's clock.
    pub fn new(clock: Box<dyn Fn() -> f64>) -> Self {
        Player {
            lazy: LazyLoop::new(),
            clock,
            scene: None,
            product: None,
            view: None,
            drawing: [1, 1],
            css: [1.0, 1.0],
            resize_pending: false,
            scene_dirty: false,
            ready_pending: false,
            view_unreported: false,
            switch_started: None,
            last_switch_ms: None,
            script_ms: 0.0,
            script_frames: 0,
            destroyed: false,
            skip_streak: 0,
            retry_requested: false,
            counters: DrawCounters::default(),
            events: Vec::new(),
            warm_pipelines: None,
        }
    }

    /// Takes the waiting events.
    pub fn drain_events(&mut self) -> Vec<PlayerEvent> {
        std::mem::take(&mut self.events)
    }

    /// Records a failure as an `error` event and hands it back for a throw or a rejection.
    pub fn report(&mut self, error: WebError) -> WebError {
        self.events.push(PlayerEvent::Error(error.clone()));
        error
    }

    fn live(&mut self) -> Result<(), WebError> {
        if self.destroyed {
            return Err(self.report(WebError::new("destroyed", "the player was destroyed")));
        }
        Ok(())
    }

    /// The loop's state.
    pub fn lazy(&self) -> &LazyLoop {
        &self.lazy
    }

    /// Whether a product is open.
    pub fn has_product(&self) -> bool {
        self.product.is_some()
    }

    /// Whether a scene (a device and a surface) is attached.
    pub fn has_scene(&self) -> bool {
        self.scene.is_some()
    }

    /// Attaches the renderer, on creation and after a lost device came back: shows the open product
    /// again and draws.
    pub fn attach_scene(&mut self, mut scene: Box<dyn SceneBackend>) -> Schedule {
        // A restore that finishes after `destroy` must not bring the GPU objects back.
        if self.destroyed {
            return Schedule::Nothing;
        }
        scene.resize(self.drawing[0], self.drawing[1]);
        if let Some(product) = self.product.as_deref()
            && let Err(e) = scene.sync_product(product, true)
        {
            self.events.push(PlayerEvent::Error(e));
        }
        self.scene = Some(scene);
        self.warm_pipelines = None;
        self.resize_pending = false;
        self.scene_dirty = true;
        self.skip_streak = 0;
        self.lazy.invalidate()
    }

    /// The device or context is gone: drops the renderer and stops drawing. The host cancels the browser
    /// frame it had requested, so none is pending any more. The product stays in memory, so
    /// [`Player::attach_scene`] restores everything and asks for a frame.
    pub fn device_lost(&mut self, why: &str) {
        self.lazy.forget_pending();
        self.retry_requested = false;
        self.scene = None;
        self.events
            .push(PlayerEvent::Error(WebError::new("device_lost", why)));
    }

    /// The host could not request the browser frame a [`Schedule::Request`] asked for (or cancelled it):
    /// nothing is pending, so the next change asks again instead of waiting for a frame that never comes.
    pub fn frame_not_requested(&mut self) {
        self.lazy.forget_pending();
        self.retry_requested = false;
    }

    /// Installs an opened product, replacing the one shown. The first frame follows from
    /// [`Player::draw_first`].
    pub fn install(
        &mut self,
        product: Box<dyn ProductModel>,
        mut view: Box<dyn ViewModel>,
    ) -> Result<(), WebError> {
        self.live()?;
        view.set_viewport(self.drawing[0] as f32, self.drawing[1] as f32);
        view.set_pixel_ratio(self.drawing[0] as f32 / self.css[0].max(1.0));
        if let Some(scene) = self.scene.as_deref_mut()
            && let Err(e) = scene.sync_product(product.as_ref(), true)
        {
            return Err(self.report(e));
        }
        self.product = Some(product);
        self.view = Some(view);
        self.warm_pipelines = None;
        self.scene_dirty = true;
        self.ready_pending = true;
        self.view_unreported = false;
        self.skip_streak = 0;
        Ok(())
    }

    /// Draws the first frame of a newly installed product, right away instead of waiting for the next
    /// browser frame. When the surface skipped that frame, a browser frame is requested to try again
    /// (the host does what the returned [`Schedule`] says), so `ready` never waits for an input.
    pub fn draw_first(&mut self) -> Schedule {
        self.work(crate::lazy::FIRST_STEP_S);
        self.retry_requested = self.retry_skipped();
        if self.retry_requested {
            self.lazy.invalidate()
        } else {
            Schedule::Nothing
        }
    }

    /// Whether the last frame was skipped by the surface with the scene still to draw, and the retries
    /// ([`MAX_SKIPPED_RETRIES`]) are not used up.
    fn retry_skipped(&self) -> bool {
        self.scene_dirty && self.scene.is_some() && (1..=MAX_SKIPPED_RETRIES).contains(&self.skip_streak)
    }

    /// The JS `options()`.
    pub fn options(&mut self) -> Result<Value, WebError> {
        self.live()?;
        match self.product.as_deref() {
            Some(p) => Ok(p.options()),
            None => Err(self.report(no_product())),
        }
    }

    /// The JS `build()`.
    pub fn build(&mut self) -> Result<Value, WebError> {
        self.live()?;
        let view = self.view_values();
        let Some(p) = self.product.as_deref() else {
            return Err(self.report(no_product()));
        };
        p.build(&view).map_err(|e| self.report(e))
    }

    fn view_values(&self) -> ViewValues {
        self.view.as_deref().map(|v| v.shown()).unwrap_or_default()
    }

    /// The JS `setPick`. A refusal changes nothing.
    pub fn set_pick(
        &mut self,
        group: &str,
        value: &Value,
        adjust: bool,
    ) -> Result<(PickResult, Schedule), WebError> {
        self.live()?;
        let view = self.view_values();
        let Some(product) = self.product.as_deref_mut() else {
            return Err(self.report(no_product()));
        };
        let started = if self.lazy.manual() {
            self.script_ms
        } else {
            (self.clock)()
        };
        let applied = match product.set_pick(group, value, adjust) {
            Ok(a) => a,
            Err(e) => {
                self.counters.refused += 1;
                return Err(self.report(e));
            }
        };
        let build = match product.build(&view) {
            Ok(b) => b,
            Err(e) => return Err(self.report(e)),
        };
        if let Some(scene) = self.scene.as_deref_mut()
            && let Some(product) = self.product.as_deref()
            && let Err(e) = scene.sync_product(product, applied.geometry_changed)
        {
            return Err(self.report(e));
        }
        self.counters.picks += 1;
        self.switch_started = Some(started);
        self.scene_dirty = true;
        self.events.push(PlayerEvent::Changed {
            cause: "pick",
            build: build.clone(),
        });
        let schedule = self.lazy.invalidate();
        Ok((
            PickResult {
                build,
                adjusted: applied.adjusted,
            },
            schedule,
        ))
    }

    /// The JS `setView`: clamps, never fails on values. Returns where the view ends up and whether the
    /// limits changed the request.
    pub fn set_view(&mut self, request: &ViewRequest) -> Result<(Value, Schedule), WebError> {
        self.live()?;
        let Some(view) = self.view.as_deref_mut() else {
            return Err(self.report(no_product()));
        };
        let wanted = request.over(view.shown());
        let immediate = !request.animate.unwrap_or(true);
        let target = view.set_view(wanted, immediate);
        let clamped = !close(&wanted, &target);
        self.view_unreported = true;
        let report = json!({"view": view_json(&target), "clamped": clamped});
        Ok((report, self.lazy.invalidate()))
    }

    /// The JS `view()`.
    pub fn view(&mut self) -> Result<Value, WebError> {
        self.live()?;
        match self.view.as_deref() {
            Some(v) => Ok(view_json(&v.shown())),
            None => Err(self.report(no_product())),
        }
    }

    /// The JS `resetView()`.
    pub fn reset_view(&mut self) -> Result<Schedule, WebError> {
        self.live()?;
        let Some(view) = self.view.as_deref_mut() else {
            return Err(self.report(no_product()));
        };
        view.reset();
        self.view_unreported = true;
        Ok(self.lazy.invalidate())
    }

    fn input(&mut self, f: impl FnOnce(&mut dyn ViewModel)) -> Schedule {
        let Some(view) = self.view.as_deref_mut() else {
            return Schedule::Nothing;
        };
        f(view);
        self.lazy.invalidate()
    }

    /// The time of a pointer event: the script's clock in deterministic mode, so a scripted drag flings the
    /// same way every run whenever its events reach the player.
    fn input_time(&self, t_ms: f64) -> f64 {
        if self.lazy.manual() { self.script_ms } else { t_ms }
    }

    /// A pointer went down on the canvas (device pixels).
    pub fn pointer_down(&mut self, id: u64, class: PointerClass, x: f32, y: f32, t_ms: f64) -> Schedule {
        let t_ms = self.input_time(t_ms);
        self.input(|v| v.pointer_down(id, class, x, y, t_ms))
    }

    /// A pointer moved.
    pub fn pointer_move(&mut self, id: u64, x: f32, y: f32, t_ms: f64) -> Schedule {
        let t_ms = self.input_time(t_ms);
        self.input(|v| v.pointer_move(id, x, y, t_ms))
    }

    /// A pointer lifted.
    pub fn pointer_up(&mut self, id: u64, t_ms: f64) -> Schedule {
        let t_ms = self.input_time(t_ms);
        self.input(|v| v.pointer_up(id, t_ms))
    }

    /// A pointer was cancelled.
    pub fn pointer_cancel(&mut self, id: u64) -> Schedule {
        self.input(|v| v.pointer_cancel(id))
    }

    /// A wheel scroll in lines; positive zooms in.
    pub fn wheel(&mut self, lines: f32) -> Schedule {
        self.input(|v| v.wheel(lines))
    }

    /// The canvas changed size. Same size: nothing happens.
    pub fn resize(&mut self, css: [f32; 2], drawing: [u32; 2]) -> Schedule {
        if drawing == self.drawing && css == self.css {
            return Schedule::Nothing;
        }
        self.css = css;
        self.drawing = drawing;
        self.resize_pending = true;
        self.scene_dirty = true;
        if let Some(view) = self.view.as_deref_mut() {
            view.set_viewport(drawing[0] as f32, drawing[1] as f32);
            view.set_pixel_ratio(drawing[0] as f32 / css[0].max(1.0));
        }
        self.lazy.invalidate()
    }

    /// Draws right now if the page shows the canvas: a resize has just cleared it, and waiting for the next
    /// browser frame would show one empty frame. Returns whether it drew.
    pub fn draw_now(&mut self) -> bool {
        if !self.lazy.visible() || self.lazy.manual() || self.destroyed {
            return false;
        }
        self.work(0.0)
    }

    /// Something the host draws over the scene changed (an option panel): draw one more frame.
    pub fn invalidate(&mut self) -> Schedule {
        self.scene_dirty = true;
        self.lazy.invalidate()
    }

    /// The drawing size in device pixels.
    pub fn drawing_size(&self) -> [u32; 2] {
        self.drawing
    }

    /// The page became visible or hidden.
    pub fn set_visible(&mut self, visible: bool) -> Schedule {
        if visible && !self.lazy.visible() {
            self.scene_dirty = true;
        }
        self.lazy.set_visible(visible)
    }

    /// Scripted capture: frames come only from [`Player::step`] and [`Player::step_forced`], with the step the
    /// script gives, and the script's clock replaces the player's, so the same script makes the same frames.
    /// Turning it on starts that clock at 0; repeating a state changes nothing.
    pub fn set_deterministic(&mut self, on: bool) -> Schedule {
        if on != self.lazy.manual() {
            self.switch_started = None;
            self.script_ms = 0.0;
            self.script_frames = 0;
        }
        self.lazy.set_manual(on)
    }

    /// One scripted frame of `dt_ms`: draws if something moved or changed, as the lazy loop would. Returns
    /// whether it drew. Only in deterministic mode.
    pub fn step(&mut self, dt_ms: f64) -> Result<bool, WebError> {
        self.advance(dt_ms, false)
    }

    /// One scripted frame of `dt_ms` that draws even when nothing changed, as a video needs a frame at every
    /// step. Only in deterministic mode. Returns whether a frame reached the target.
    pub fn step_forced(&mut self, dt_ms: f64) -> Result<bool, WebError> {
        self.advance(dt_ms, true)
    }

    fn advance(&mut self, dt_ms: f64, force: bool) -> Result<bool, WebError> {
        self.live()?;
        if !self.lazy.manual() {
            return Err(self.report(WebError::new("bad_argument", "step needs deterministic mode")));
        }
        if !dt_ms.is_finite() || dt_ms < 0.0 {
            return Err(self.report(WebError::new("bad_argument", "step needs a time of 0 or more")));
        }
        self.script_ms += dt_ms;
        self.script_frames += 1;
        if force {
            self.scene_dirty = true;
        }
        Ok(self.work((dt_ms / 1000.0) as f32))
    }

    /// The script's clock in deterministic mode: frames stepped and ms stepped since it was turned on.
    pub fn script_clock(&self) -> (u64, f64) {
        (self.script_frames, self.script_ms)
    }

    /// The camera the next draw uses, when a product is open.
    pub fn pose(&self) -> Option<CameraPose> {
        self.view.as_deref().map(|v| v.pose())
    }

    /// A browser frame arrived.
    pub fn frame(&mut self, now_ms: f64) -> Schedule {
        let Some(dt) = self.lazy.frame_begin(now_ms) else {
            return Schedule::Nothing;
        };
        // A frame that is not only a retry carries a change: the retries start over.
        if !self.retry_requested {
            self.skip_streak = 0;
        }
        self.work(dt);
        // A frame the surface skipped is drawn again on the next browser frame.
        self.retry_requested = self.retry_skipped();
        let again = self.animating() || self.retry_requested;
        self.lazy.frame_end(again)
    }

    /// Whether the view still moves without input.
    pub fn animating(&self) -> bool {
        self.view.as_deref().is_some_and(|v| v.is_animating())
    }

    /// One frame's work. Draws only if something moved or changed; returns whether it drew.
    fn work(&mut self, dt_s: f32) -> bool {
        let _span = tracing::info_span!("web_player_frame").entered();
        let moved = self.view.as_deref_mut().is_some_and(|v| v.update(dt_s));
        if moved {
            self.view_unreported = true;
        }
        if self.resize_pending
            && let Some(scene) = self.scene.as_deref_mut()
        {
            scene.resize(self.drawing[0], self.drawing[1]);
            self.resize_pending = false;
        }
        let mut drew = false;
        if moved || self.scene_dirty {
            let pose = self.view.as_deref().map(|v| v.pose());
            if let (Some(scene), Some(pose)) = (self.scene.as_deref_mut(), pose) {
                match scene.draw(&pose) {
                    Ok(DrawOutcome::Drawn) => {
                        self.counters.draws += 1;
                        self.scene_dirty = false;
                        self.skip_streak = 0;
                        drew = true;
                    }
                    Ok(DrawOutcome::Skipped) => {
                        self.counters.skipped += 1;
                        self.skip_streak = self.skip_streak.saturating_add(1);
                        // The surface still shows the previous picture. A gesture can settle on this
                        // same frame, which clears `animating`, so the retry has to see the scene as dirty.
                        self.scene_dirty = true;
                    }
                    Err(e) => {
                        self.scene_dirty = false;
                        self.skip_streak = 0;
                        self.events.push(PlayerEvent::Error(e));
                    }
                }
            }
        }
        if drew {
            if self.warm_pipelines.is_none() && self.product.is_some() {
                self.warm_pipelines = self.scene.as_deref().map(|s| s.pipelines());
            }
            if let Some(started) = self.switch_started.take() {
                let now = if self.lazy.manual() {
                    self.script_ms
                } else {
                    (self.clock)()
                };
                self.last_switch_ms = Some(now - started);
            }
            if self.ready_pending {
                self.ready_pending = false;
                let incomplete = self.product.as_deref().is_some_and(|p| p.incomplete());
                self.events.push(PlayerEvent::Ready { incomplete });
            }
        }
        if self.view_unreported && !self.animating() {
            self.view_unreported = false;
            let view = self.view_values();
            if let Some(product) = self.product.as_deref() {
                match product.build(&view) {
                    Ok(build) => self.events.push(PlayerEvent::Changed { cause: "view", build }),
                    Err(e) => self.events.push(PlayerEvent::Error(e)),
                }
            }
        }
        drew
    }

    /// Ends the player: stops the loop and drops the GPU objects and the product.
    pub fn destroy(&mut self) {
        self.lazy.stop();
        self.scene = None;
        self.product = None;
        self.view = None;
        self.destroyed = true;
    }

    /// Whether [`Player::destroy`] ran.
    pub fn destroyed(&self) -> bool {
        self.destroyed
    }

    /// The numbers the JS `stats()` reports.
    pub fn stats(&self, backend: &str) -> Value {
        let LoopCounters { requests, frames } = self.lazy.counters();
        json!({
            "backend": backend,
            "requests": requests,
            "frames": frames,
            "draws": self.counters.draws,
            "skipped": self.counters.skipped,
            "picks": self.counters.picks,
            "refused": self.counters.refused,
            "pending": self.lazy.pending(),
            "dirty": self.lazy.dirty(),
            "visible": self.lazy.visible(),
            "deterministic": self.lazy.manual(),
            "scriptFrame": self.script_frames,
            "scriptClockMs": self.script_ms,
            "pipelines": self.scene.as_deref().map_or(0, |s| s.pipelines()),
            "looks": self.scene.as_deref().is_some_and(|s| s.draws_looks()),
            "lastSwitchMs": self.last_switch_ms,
            "drawing": self.drawing,
            "problems": self.problems(),
        })
    }

    /// The look problems of the current picks, plus `warm_set_miss` when the scene created pipelines
    /// after the first frame (every pipeline belongs to the warm set built during `open`, spec 42 § 7).
    fn problems(&self) -> Vec<String> {
        let mut problems = self
            .product
            .as_deref()
            .and_then(|p| p.plan().ok())
            .map(|p| p.problems)
            .unwrap_or_default();
        if let (Some(warm), Some(scene)) = (self.warm_pipelines, self.scene.as_deref()) {
            let now = scene.pipelines();
            if now > warm {
                problems.push(format!(
                    "warm_set_miss: {} pipelines created after the first frame ({warm} warm, {now} now)",
                    now - warm
                ));
            }
        }
        problems
    }
}

fn no_product() -> WebError {
    WebError::new("no_product", "open a product first")
}

fn close(a: &ViewValues, b: &ViewValues) -> bool {
    const E: f32 = 1e-3;
    (a.tilt[0] - b.tilt[0]).abs() < E
        && (a.tilt[1] - b.tilt[1]).abs() < E
        && (a.spin - b.spin).abs() < E
        && (a.pan[0] - b.pan[0]).abs() < E
        && (a.pan[1] - b.pan[1]).abs() < E
        && (a.zoom - b.zoom).abs() < E
}

fn view_json(v: &ViewValues) -> Value {
    json!({"tilt": v.tilt, "spin": v.spin, "pan": v.pan, "zoom": v.zoom})
}
