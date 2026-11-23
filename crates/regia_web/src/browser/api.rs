// SPDX-License-Identifier: MPL-2.0
//! The page's API: the `RegiaViewer` class, and the loop and input wiring behind it.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};

use serde_json::{Value, json};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{future_to_promise, spawn_local};
use web_sys::{
    CustomEvent, CustomEventInit, Event, HtmlCanvasElement, PointerEvent, ResizeObserver, WheelEvent,
};

use super::dom::Listener;
use super::fetch::{fetch_all, fetch_bytes};
use super::gpu;
use super::js::{describe, error_value, from_js, to_js};
use crate::player::{Player, PlayerEvent};
use crate::product::PackageProduct;
use crate::scene::ViewerScene;
use crate::seams::{PointerClass, ViewRequest};
use crate::view::ProductView;
use crate::{PlayerOptions, Schedule, WebError, drawing_size, package, urls};

thread_local! {
    /// Live players by id. wgpu's device-lost callback must be `Send`, so it carries the id and finds its
    /// player here; the module has one thread.
    static LIVE: RefCell<BTreeMap<u32, Weak<Inner>>> = const { RefCell::new(BTreeMap::new()) };
    static NEXT_ID: Cell<u32> = const { Cell::new(1) };
}

const LINES_PER_PIXEL: f32 = 1.0 / 100.0;
/// The environment file in a studio folder.
const STUDIO_ENV_FILE: &str = "env.renv";

type FrameClosure = Closure<dyn FnMut(f64)>;

struct Inner {
    id: u32,
    player: RefCell<Player>,
    canvas: HtmlCanvasElement,
    options: PlayerOptions,
    backend: Cell<&'static str>,
    listeners: RefCell<Vec<(String, js_sys::Function)>>,
    raf_closure: RefCell<Option<FrameClosure>>,
    raf_id: Cell<Option<i32>>,
    dom: RefCell<Vec<Listener>>,
    resize_closure: RefCell<Option<Closure<dyn FnMut()>>>,
    observer: RefCell<Option<ResizeObserver>>,
    pressed: RefCell<Vec<i32>>,
    /// `open()` promises waiting for their first frame: (the open's generation, resolve, reject).
    ready_waiters: RefCell<Vec<(u32, js_sys::Function, js_sys::Function)>>,
    open_generation: Cell<u32>,
    /// The generation of the open whose product the player shows.
    installed_generation: Cell<u32>,
    restoring: Cell<bool>,
    studio: regia_viewer::BakedEnv,
    studio_id: String,
}

impl Drop for Inner {
    /// A player that goes away without `destroy()` (a `create()` that failed after wiring the canvas, or a
    /// viewer the page dropped): the canvas keeps its `ResizeObserver` alive, so it is disconnected here, or a
    /// later resize would call the freed closure and throw.
    fn drop(&mut self) {
        if let Some(observer) = self.observer.get_mut().take() {
            observer.disconnect();
        }
        if let Some(id) = self.raf_id.take()
            && let Some(window) = web_sys::window()
        {
            let _ = window.cancel_animation_frame(id);
        }
        let id = self.id;
        let _ = LIVE.try_with(|live| {
            if let Ok(mut live) = live.try_borrow_mut() {
                live.remove(&id);
            }
        });
    }
}

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map_or(0.0, |p| p.now())
}

impl Inner {
    /// Runs `f` on the player, then does what it scheduled and tells the page, with the borrow released so
    /// a listener may call straight back in.
    fn with<T>(self: &Rc<Self>, f: impl FnOnce(&mut Player) -> T) -> T {
        let out = f(&mut self.player.borrow_mut());
        self.flush_events();
        out
    }

    fn apply(self: &Rc<Self>, schedule: Schedule) {
        if schedule == Schedule::Request {
            self.request_frame();
        }
    }

    fn request_frame(self: &Rc<Self>) {
        let requested = {
            let closure = self.raf_closure.borrow();
            match (web_sys::window(), closure.as_ref()) {
                (Some(window), Some(closure)) => {
                    match window.request_animation_frame(closure.as_ref().unchecked_ref()) {
                        Ok(id) => {
                            self.raf_id.set(Some(id));
                            true
                        }
                        Err(e) => {
                            web_sys::console::error_1(&e);
                            false
                        }
                    }
                }
                _ => false,
            }
        };
        // No frame is on its way: the loop must not wait for one, so the next change asks again.
        if !requested {
            self.player.borrow_mut().frame_not_requested();
        }
    }

    fn cancel_frame(&self) {
        if let (Some(id), Some(window)) = (self.raf_id.take(), web_sys::window()) {
            let _ = window.cancel_animation_frame(id);
        }
    }

    fn on_frame(self: &Rc<Self>, now: f64) {
        self.raf_id.set(None);
        let schedule = self.with(|p| p.frame(now));
        self.apply(schedule);
    }

    fn flush_events(self: &Rc<Self>) {
        let events = self.player.borrow_mut().drain_events();
        for event in events {
            let (name, body) = match &event {
                PlayerEvent::Ready { incomplete } => (
                    "ready",
                    json!({"type": "ready", "incomplete": incomplete, "backend": self.backend.get()}),
                ),
                PlayerEvent::Changed { cause, build } => (
                    "changed",
                    json!({"type": "changed", "cause": cause, "build": build}),
                ),
                PlayerEvent::Error(e) => (
                    "error",
                    json!({"type": "error", "code": e.code, "message": e.message}),
                ),
            };
            match &event {
                // The shown product's open resolves; an older open a later one replaced is superseded, not
                // ready (its product is not the one on the canvas).
                PlayerEvent::Ready { .. } => {
                    let shown = self.installed_generation.get();
                    let superseded = error_value(&WebError::new(
                        "open_superseded",
                        "a later open() replaced this one",
                    ));
                    for (generation, resolve, reject) in std::mem::take(&mut *self.ready_waiters.borrow_mut())
                    {
                        if generation == shown {
                            let _ = resolve.call0(&JsValue::NULL);
                        } else {
                            let _ = reject.call1(&JsValue::NULL, &superseded);
                        }
                    }
                }
                // A first frame that can no longer come must not leave open() waiting. A lost device is not
                // that: its restore draws the frame. Nor is a refused package: an open that fails is refused
                // before it waits (its own promise carries the error), and a pick the package cannot draw
                // changes nothing, so neither may reject an open still waiting for its frame.
                PlayerEvent::Error(e) if matches!(e.code, "render_failed" | "backend_unavailable") => {
                    for (_, _, reject) in std::mem::take(&mut *self.ready_waiters.borrow_mut()) {
                        let _ = reject.call1(&JsValue::NULL, &error_value(e));
                    }
                }
                _ => {}
            }
            self.dispatch(name, &body);
        }
    }

    fn dispatch(&self, name: &str, body: &Value) {
        let Ok(detail) = to_js(body) else {
            return;
        };
        // A copy: a listener may call `on` or `off`.
        let listeners: Vec<js_sys::Function> = self
            .listeners
            .borrow()
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, f)| f.clone())
            .collect();
        for f in listeners {
            if let Err(e) = f.call1(&JsValue::NULL, &detail) {
                web_sys::console::error_1(&e);
            }
        }
        let init = CustomEventInit::new();
        init.set_detail(&detail);
        if let Ok(event) = CustomEvent::new_with_event_init_dict(&format!("regia-{name}"), &init) {
            let _ = self.canvas.dispatch_event(&event);
        }
    }

    fn css_size(&self) -> [f32; 2] {
        let rect = self.canvas.get_bounding_client_rect();
        [rect.width() as f32, rect.height() as f32]
    }

    fn drawing_for(&self, css: [f32; 2]) -> [u32; 2] {
        let dpr = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio()) as f32;
        drawing_size(css, dpr, self.options.max_pixel_ratio)
    }

    fn on_resize(self: &Rc<Self>) {
        let css = self.css_size();
        let drawing = self.drawing_for(css);
        // Setting these clears the canvas even to the same value, so only a real change touches them, and the
        // new frame goes out before the browser paints an empty one.
        let changed = self.canvas.width() != drawing[0] || self.canvas.height() != drawing[1];
        if changed {
            self.canvas.set_width(drawing[0]);
            self.canvas.set_height(drawing[1]);
        }
        let schedule = self.with(|p| {
            let schedule = p.resize(css, drawing);
            if changed {
                p.draw_now();
            }
            schedule
        });
        self.apply(schedule);
    }

    /// Where a pointer is on the canvas, in drawing pixels, the view's unit.
    fn pointer_xy(&self, e: &PointerEvent) -> (f32, f32) {
        let rect = self.canvas.get_bounding_client_rect();
        let scale_x = f64::from(self.canvas.width()) / rect.width().max(1.0);
        let scale_y = f64::from(self.canvas.height()) / rect.height().max(1.0);
        (
            ((f64::from(e.client_x()) - rect.left()) * scale_x) as f32,
            ((f64::from(e.client_y()) - rect.top()) * scale_y) as f32,
        )
    }

    fn on_pointer_down(self: &Rc<Self>, e: &PointerEvent) {
        e.prevent_default();
        let id = e.pointer_id();
        // Capture fails for an untrusted (scripted) pointer; the path works without it.
        let _ = self.canvas.set_pointer_capture(id);
        self.pressed.borrow_mut().push(id);
        let (x, y) = self.pointer_xy(e);
        let class = match (e.pointer_type().as_str(), e.button()) {
            ("mouse", 0) => PointerClass::Mouse,
            ("mouse", _) => PointerClass::MouseSecondary,
            _ => PointerClass::Touch,
        };
        let t = e.time_stamp();
        let schedule = self.with(|p| p.pointer_down(id as u32 as u64, class, x, y, t));
        self.apply(schedule);
    }

    fn on_pointer_move(self: &Rc<Self>, e: &PointerEvent) {
        let id = e.pointer_id();
        // Hover wakes nothing: the loop must stay idle unless a pointer is down.
        if !self.pressed.borrow().contains(&id) {
            return;
        }
        let (x, y) = self.pointer_xy(e);
        let t = e.time_stamp();
        let schedule = self.with(|p| p.pointer_move(id as u32 as u64, x, y, t));
        self.apply(schedule);
    }

    fn release(&self, id: i32) -> bool {
        let mut pressed = self.pressed.borrow_mut();
        match pressed.iter().position(|p| *p == id) {
            Some(i) => {
                pressed.swap_remove(i);
                true
            }
            None => false,
        }
    }

    fn on_pointer_up(self: &Rc<Self>, e: &PointerEvent) {
        let id = e.pointer_id();
        if self.release(id) {
            let t = e.time_stamp();
            let schedule = self.with(|p| p.pointer_up(id as u32 as u64, t));
            self.apply(schedule);
        }
    }

    fn on_pointer_cancel(self: &Rc<Self>, e: &PointerEvent) {
        let id = e.pointer_id();
        if self.release(id) {
            let schedule = self.with(|p| p.pointer_cancel(id as u32 as u64));
            self.apply(schedule);
        }
    }

    fn on_wheel(self: &Rc<Self>, e: &WheelEvent) {
        e.prevent_default();
        let lines = match e.delta_mode() {
            WheelEvent::DOM_DELTA_PIXEL => e.delta_y() as f32 * LINES_PER_PIXEL,
            WheelEvent::DOM_DELTA_LINE => e.delta_y() as f32,
            _ => e.delta_y() as f32 * 10.0,
        };
        // A positive delta scrolls down, which zooms out.
        let schedule = self.with(|p| p.wheel(-lines));
        self.apply(schedule);
    }

    fn on_visibility(self: &Rc<Self>) {
        let hidden = web_sys::window()
            .and_then(|w| w.document())
            .is_some_and(|d| d.hidden());
        let schedule = self.with(|p| p.set_visible(!hidden));
        if hidden {
            self.cancel_frame();
        }
        self.apply(schedule);
    }

    fn lost(self: &Rc<Self>, why: &str) {
        self.cancel_frame();
        self.with(|p| p.device_lost(why));
    }

    /// Makes a new device and scene and attaches them: on creation and after a loss.
    async fn attach_gpu(self: &Rc<Self>) -> Result<(), WebError> {
        let size = self.player.borrow().drawing_size();
        // A restore asks for the API the canvas already holds: a canvas keeps the first context kind it handed
        // out, so `auto` trying WebGPU again on a WebGL2 canvas could only fail.
        let choice = match self.backend.get() {
            "webgpu" => crate::BackendChoice::Webgpu,
            "webgl2" => crate::BackendChoice::Webgl2,
            _ => self.options.backend,
        };
        let gpu = gpu::create(&self.canvas, choice, size).await?;
        self.backend.set(gpu.backend);
        let format = gpu.format();
        let sample_count = match self.options.msaa {
            1 => 1,
            _ => regia_viewer::gpu::sample_count_for(&gpu.adapter, format),
        };
        let device = gpu.device.clone();
        let queue = gpu.queue.clone();
        let config = crate::scene::player_config(format, sample_count);
        let scene = ViewerScene::new(device.clone(), queue, config, &self.studio, gpu)?;
        let id = self.id;
        // wgpu's default for an uncaptured error is a panic, which would end the module and the page's player
        // with it. It may arrive while the player is drawing, so the report waits for the next task.
        device.on_uncaptured_error(std::sync::Arc::new(move |error: wgpu::Error| {
            let message = error.to_string();
            web_sys::console::error_1(&message.clone().into());
            spawn_local(async move {
                let player = LIVE.with(|live| live.borrow().get(&id).and_then(Weak::upgrade));
                if let Some(player) = player {
                    player.with(|p| {
                        p.report(WebError::new("render_failed", message));
                    });
                }
            });
        }));
        // A loss can be reported from inside a wgpu call the player is making (a WebGL2 context lost
        // during `configure` or `get_current_texture`), while the player is borrowed: like the error
        // report above, the handling waits for the next task instead of borrowing it again here.
        device.set_device_lost_callback(move |reason, message| {
            if reason != wgpu::DeviceLostReason::Destroyed {
                spawn_local(async move {
                    let player = LIVE.with(|live| live.borrow().get(&id).and_then(Weak::upgrade));
                    if let Some(player) = player {
                        player.lost(&message);
                        Inner::restore(&player);
                    }
                });
            }
        });
        let schedule = self.with(|p| p.attach_scene(Box::new(scene)));
        self.apply(schedule);
        Ok(())
    }

    /// One attempt to come back from a loss.
    fn restore(self: &Rc<Self>) {
        if self.restoring.replace(true) {
            return;
        }
        let this = self.clone();
        spawn_local(async move {
            let result = this.attach_gpu().await;
            this.restoring.set(false);
            if let Err(e) = result {
                this.with(|p| {
                    p.report(e);
                });
            }
        });
    }

    fn wire_dom(self: &Rc<Self>) {
        let target: &web_sys::EventTarget = self.canvas.as_ref();
        let weak = Rc::downgrade(self);
        let on = move |f: fn(&Rc<Inner>, &Event)| {
            let weak = weak.clone();
            move |e: Event| {
                if let Some(this) = weak.upgrade() {
                    f(&this, &e);
                }
            }
        };
        fn pointer(this: &Rc<Inner>, e: &Event, f: fn(&Rc<Inner>, &PointerEvent)) {
            if let Some(e) = e.dyn_ref::<PointerEvent>() {
                f(this, e);
            }
        }
        let mut dom = vec![
            Listener::new(
                target,
                "pointerdown",
                false,
                on(|t, e| pointer(t, e, Inner::on_pointer_down)),
            ),
            Listener::new(
                target,
                "pointermove",
                true,
                on(|t, e| pointer(t, e, Inner::on_pointer_move)),
            ),
            Listener::new(
                target,
                "pointerup",
                true,
                on(|t, e| pointer(t, e, Inner::on_pointer_up)),
            ),
            Listener::new(
                target,
                "pointercancel",
                true,
                on(|t, e| pointer(t, e, Inner::on_pointer_cancel)),
            ),
            Listener::new(
                target,
                "wheel",
                false,
                on(|t, e| {
                    if let Some(e) = e.dyn_ref::<WheelEvent>() {
                        t.on_wheel(e);
                    }
                }),
            ),
            Listener::new(target, "contextmenu", false, on(|_, e| e.prevent_default())),
            Listener::new(
                target,
                "webglcontextlost",
                false,
                on(|t, e| {
                    // Without this the browser never restores the context.
                    e.prevent_default();
                    t.lost("the WebGL context was lost");
                }),
            ),
            Listener::new(target, "webglcontextrestored", true, on(|t, _| Inner::restore(t))),
        ];
        if let Some(window) = web_sys::window() {
            // A browser zoom changes the pixel ratio with no change in CSS size that an observer would see.
            dom.push(Listener::new(
                window.as_ref(),
                "resize",
                true,
                on(|t, _| t.on_resize()),
            ));
            if let Some(document) = window.document() {
                dom.push(Listener::new(
                    document.as_ref(),
                    "visibilitychange",
                    true,
                    on(|t, _| t.on_visibility()),
                ));
            }
        }
        *self.dom.borrow_mut() = dom;

        let weak = Rc::downgrade(self);
        let closure = Closure::<dyn FnMut()>::new(move || {
            if let Some(this) = weak.upgrade() {
                this.on_resize();
            }
        });
        match ResizeObserver::new(closure.as_ref().unchecked_ref()) {
            Ok(observer) => {
                observer.observe(self.canvas.as_ref());
                *self.observer.borrow_mut() = Some(observer);
            }
            Err(e) => web_sys::console::error_1(&e),
        }
        *self.resize_closure.borrow_mut() = Some(closure);

        let weak = Rc::downgrade(self);
        *self.raf_closure.borrow_mut() = Some(Closure::new(move |now: f64| {
            if let Some(this) = weak.upgrade() {
                this.on_frame(now);
            }
        }));
    }

    async fn fetch_package(url: &str) -> Result<regia_viewer_pack::Package, WebError> {
        let manifest_url = urls::join(url, package::MANIFEST)?;
        let manifest = fetch_bytes(&manifest_url, "package_fetch_failed").await?;
        let files = package::files_to_fetch(url, &manifest)?;
        let urls: Vec<String> = files.iter().map(|(_, u)| u.clone()).collect();
        let bodies = fetch_all(&urls, "package_fetch_failed").await?;
        let by_name: BTreeMap<String, Vec<u8>> =
            files.into_iter().map(|(name, _)| name).zip(bodies).collect();
        package::verify(&manifest, &by_name)
    }

    async fn open(self: &Rc<Self>, url: String) -> Result<(), WebError> {
        let generation = self.open_generation.get() + 1;
        self.open_generation.set(generation);
        let package = Self::fetch_package(&url).await?;
        if generation != self.open_generation.get() {
            return Err(WebError::new(
                "open_superseded",
                "a later open() replaced this one",
            ));
        }
        let mut product = PackageProduct::open(package, &self.studio_id)?;
        let view = ProductView::for_product(&mut product)?;
        self.player
            .borrow_mut()
            .install(Box::new(product), Box::new(view))?;
        self.installed_generation.set(generation);
        let waiter = js_sys::Promise::new(&mut |resolve, reject| {
            self.ready_waiters
                .borrow_mut()
                .push((generation, resolve, reject))
        });
        let schedule = self.with(|p| p.draw_first());
        self.apply(schedule);
        // No device (a restore after a loss failed, so none is coming): try one now. Its first frame settles
        // the waiter, and its failure (`backend_unavailable`, `render_failed`) rejects it instead of leaving
        // open() waiting forever. While a restore runs this does nothing; that restore draws the frame.
        if !self.player.borrow().has_scene() {
            Inner::restore(self);
        }
        wasm_bindgen_futures::JsFuture::from(waiter).await.map_err(|e| {
            let code = js_sys::Reflect::get(&e, &"code".into())
                .ok()
                .and_then(|c| c.as_string());
            let code = crate::ERROR_CODES
                .iter()
                .map(|(known, _)| *known)
                .find(|known| code.as_deref() == Some(known))
                .unwrap_or("render_failed");
            WebError::new(code, describe(&e))
        })?;
        Ok(())
    }
}

async fn load_studio(options: &PlayerOptions) -> Result<(regia_viewer::BakedEnv, String), WebError> {
    match &options.studio {
        None => Ok((
            regia_viewer::BakedEnv::studio_default(),
            "studio-default".to_string(),
        )),
        Some(url) => {
            let file = urls::join(url, STUDIO_ENV_FILE)?;
            let bytes = fetch_bytes(&file, "studio_fetch_failed").await?;
            let env = regia_viewer::BakedEnv::parse(&bytes)
                .map_err(|e| WebError::new("studio_invalid", format!("{file}: {e}")))?;
            Ok((
                env,
                url.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("studio")
                    .to_string(),
            ))
        }
    }
}

/// The product viewer on a canvas.
///
/// ```js
/// const viewer = await RegiaViewer.create(canvas, { backend: "auto" });
/// viewer.on("changed", e => console.log(e.build));
/// await viewer.open("products/a1/");
/// viewer.setPick("Material", "Titanium");
/// ```
#[wasm_bindgen(js_name = RegiaViewer)]
pub struct RegiaViewer {
    inner: Rc<Inner>,
}

fn throw(inner: &Rc<Inner>, error: WebError) -> JsValue {
    let error = inner.with(|p| p.report(error));
    error_value(&error)
}

#[wasm_bindgen(js_class = RegiaViewer)]
impl RegiaViewer {
    /// Creates a viewer on `canvas`: picks the API, creates the device and sets up input. Resolves to the
    /// viewer; rejects with `{code, message}`.
    pub fn create(canvas: HtmlCanvasElement, options: JsValue) -> js_sys::Promise {
        future_to_promise(async move {
            let options = from_js(&options)
                .and_then(PlayerOptions::from_value)
                .map_err(|e| error_value(&e))?;
            let (studio, studio_id) = load_studio(&options).await.map_err(|e| error_value(&e))?;
            let id = NEXT_ID.with(|n| n.replace(n.get() + 1));
            let inner = Rc::new(Inner {
                id,
                player: RefCell::new(Player::new(Box::new(now_ms))),
                canvas,
                options,
                backend: Cell::new("none"),
                listeners: RefCell::default(),
                raf_closure: RefCell::default(),
                raf_id: Cell::new(None),
                dom: RefCell::default(),
                resize_closure: RefCell::default(),
                observer: RefCell::default(),
                pressed: RefCell::default(),
                ready_waiters: RefCell::default(),
                open_generation: Cell::new(0),
                installed_generation: Cell::new(0),
                restoring: Cell::new(false),
                studio,
                studio_id,
            });
            LIVE.with(|live| live.borrow_mut().insert(id, Rc::downgrade(&inner)));
            if let Some(style) = inner.canvas.dyn_ref::<web_sys::HtmlElement>().map(|e| e.style()) {
                let _ = style.set_property("touch-action", "none");
            }
            let css = inner.css_size();
            let drawing = inner.drawing_for(css);
            inner.canvas.set_width(drawing[0]);
            inner.canvas.set_height(drawing[1]);
            let schedule = inner.player.borrow_mut().resize(css, drawing);
            inner.wire_dom();
            // The resize asked for a frame and the loop now counts one as pending: it must really go out
            // (the frame closure exists from `wire_dom` on), or every later change waits for it forever.
            inner.apply(schedule);
            // A page opened in a background tab starts hidden.
            inner.on_visibility();
            inner.attach_gpu().await.map_err(|e| error_value(&e))?;
            Ok(JsValue::from(RegiaViewer { inner }))
        })
    }

    /// Opens a product package folder, replacing what is shown. Resolves after the first frame.
    pub fn open(&self, url: String) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            match inner.open(url).await {
                Ok(()) => Ok(JsValue::UNDEFINED),
                Err(e) => Err(throw(&inner, e)),
            }
        })
    }

    /// The option groups with one-step availability.
    pub fn options(&self) -> Result<JsValue, JsValue> {
        let v = self.inner.with(|p| p.options()).map_err(|e| error_value(&e))?;
        to_js(&v)
    }

    /// Picks a value (a string, or an array for a multi group), only if the result is sold. Returns
    /// `{ok, build, adjusted}`; a refusal throws `{code, message}` and changes nothing.
    /// `opts.adjust`: find the sold build that keeps this pick and changes the fewest other groups.
    #[wasm_bindgen(js_name = setPick)]
    pub fn set_pick(&self, group: String, value: JsValue, opts: JsValue) -> Result<JsValue, JsValue> {
        let value = from_js(&value).map_err(|e| throw(&self.inner, e))?;
        let adjust = from_js(&opts).map_err(|e| throw(&self.inner, e))?["adjust"]
            .as_bool()
            .unwrap_or(false);
        let (result, schedule) = self
            .inner
            .with(|p| p.set_pick(&group, &value, adjust))
            .map_err(|e| error_value(&e))?;
        self.inner.apply(schedule);
        to_js(&json!({"ok": true, "build": result.build, "adjusted": result.adjusted}))
    }

    /// The current build.
    pub fn build(&self) -> Result<JsValue, JsValue> {
        let v = self.inner.with(|p| p.build()).map_err(|e| error_value(&e))?;
        to_js(&v)
    }

    /// Sets the view target: `{tilt, spin, pan, zoom, animate}`; absent fields stay. Limits clamp and never
    /// fail; returns `{view, clamped}`.
    #[wasm_bindgen(js_name = setView)]
    pub fn set_view(&self, view: JsValue) -> Result<JsValue, JsValue> {
        let value = from_js(&view).map_err(|e| throw(&self.inner, e))?;
        let request: ViewRequest = serde_json::from_value(value)
            .map_err(|e| throw(&self.inner, WebError::new("bad_argument", format!("view: {e}"))))?;
        let (report, schedule) = self
            .inner
            .with(|p| p.set_view(&request))
            .map_err(|e| error_value(&e))?;
        self.inner.apply(schedule);
        to_js(&report)
    }

    /// The view shown now.
    pub fn view(&self) -> Result<JsValue, JsValue> {
        let v = self.inner.with(|p| p.view()).map_err(|e| error_value(&e))?;
        to_js(&v)
    }

    /// Animates back to the default view.
    #[wasm_bindgen(js_name = resetView)]
    pub fn reset_view(&self) -> Result<(), JsValue> {
        let schedule = self.inner.with(|p| p.reset_view()).map_err(|e| error_value(&e))?;
        self.inner.apply(schedule);
        Ok(())
    }

    /// Adds a listener for `ready`, `changed` or `error`.
    pub fn on(&self, name: String, listener: js_sys::Function) -> Result<(), JsValue> {
        if !matches!(name.as_str(), "ready" | "changed" | "error") {
            return Err(error_value(&WebError::new(
                "bad_argument",
                format!("no event {name:?}"),
            )));
        }
        self.inner.listeners.borrow_mut().push((name, listener));
        Ok(())
    }

    /// Removes a listener.
    pub fn off(&self, name: String, listener: js_sys::Function) {
        self.inner
            .listeners
            .borrow_mut()
            .retain(|(n, f)| !(*n == name && js_sys::Object::is(f, &listener)));
    }

    /// Loop and draw counters, the backend, and the last switch time.
    pub fn stats(&self) -> Result<JsValue, JsValue> {
        let backend = self.inner.backend.get();
        let v = self.inner.player.borrow().stats(backend);
        to_js(&v)
    }

    /// Scripted capture: while on, frames come only from `step`, with the time the script gives, and the player's
    /// clock is the script's, so the same calls make the same frames.
    #[wasm_bindgen(js_name = setDeterministic)]
    pub fn set_deterministic(&self, on: bool) {
        if on {
            self.inner.cancel_frame();
        }
        let schedule = self.inner.with(|p| p.set_deterministic(on));
        self.inner.apply(schedule);
    }

    /// One scripted frame of `dt_ms`. Returns whether it drew. With `force`, it draws even when nothing changed, as
    /// a video needs a frame at every step. Read the canvas (or `frameHash`) in the same task.
    pub fn step(&self, dt_ms: f64, force: Option<bool>) -> Result<bool, JsValue> {
        self.inner
            .with(|p| {
                if force.unwrap_or(false) {
                    p.step_forced(dt_ms)
                } else {
                    p.step(dt_ms)
                }
            })
            .map_err(|e| error_value(&e))
    }

    /// The hash of the canvas's pixels: FNV-1a 64 of the RGBA bytes, 16 hex digits, the render-out manifest's
    /// `frame_hashes`. Call it in the task that called `step`: a canvas keeps its picture only until that task ends.
    #[wasm_bindgen(js_name = frameHash)]
    pub fn frame_hash(&self) -> Result<String, JsValue> {
        let pixels = super::dom::canvas_rgba(&self.inner.canvas)
            .map_err(|e| throw(&self.inner, WebError::new("bad_argument", e)))?;
        Ok(crate::capture::frame_hash(&pixels))
    }

    /// Stops the loop, removes every listener and drops the GPU objects. Later calls throw `destroyed`.
    pub fn destroy(&self) {
        self.inner.cancel_frame();
        self.inner.player.borrow_mut().destroy();
        self.inner.dom.borrow_mut().clear();
        if let Some(observer) = self.inner.observer.borrow_mut().take() {
            observer.disconnect();
        }
        self.inner.resize_closure.borrow_mut().take();
        self.inner.listeners.borrow_mut().clear();
        let destroyed = error_value(&WebError::new("destroyed", "the player was destroyed"));
        for (_, _, reject) in std::mem::take(&mut *self.inner.ready_waiters.borrow_mut()) {
            let _ = reject.call1(&JsValue::NULL, &destroyed);
        }
        let id = self.inner.id;
        LIVE.with(|live| live.borrow_mut().remove(&id));
    }
}
